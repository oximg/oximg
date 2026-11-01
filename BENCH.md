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

## JPEG sources since 0.11 (2026-10)

**Most tables below were measured in 2026-07, before 0.11.0.** That
includes the Linux and macOS sections, the local run of imgproxy's
harness, the AWS c8i/c9g run and the cross-format cells. The AWS
c7i/c7g tables were re-measured on 0.15.0 in 2026-10. 0.11.0 stopped
decoding JPEG sources at a reduced DCT scale by default (see
[Decode scale](#decode-scale-the-throughputquality-frontier-2026-08)),
which made every cell whose *source* is a JPEG slower: JPEG→JPEG,
JPEG→WebP and JPEG→AVIF. Those columns overstate 0.11.0 and 0.12.0.
They overstate 0.13.0 less, but still by some margin. PNG, WebP and
AVIF sources were not affected by that change.

The JPEG→JPEG cell re-measured on 0.13.0, on imgproxy's benchmark
shape: 100 DIV2K photographs from nginx, fit 512 q80, k6. This run was
on the Ryzen 7 8745HS, not AWS. The server was pinned to one SMT pair
(the c7i.large analogue) or two physical cores (the c7g.large
analogue). Configurations rotated each round. Values are req/s with
CPU ms per request in parentheses, averaged over 3 rounds at 2 VUs and
2 rounds at 8 VUs. Measured 2026-10-01; all 40 cells had 0 failed
checks.

| server | SMT pair, 2 VUs | SMT pair, 8 VUs | 2 cores, 2 VUs | 2 cores, 8 VUs |
|---|---|---|---|---|
| oximg 0.12.0 | 92.5 (21.2) | 94.0 (21.1) | 127.7 (14.8) | 135.2 (14.7) |
| **oximg 0.13.0** | **124.0** (15.7) | **127.4** (15.6) | **170.3** (10.9) | **184.5** (10.7) |
| imgproxy 4.0.17 | 115.4 (16.0) | 117.2 (16.5) | 153.8 (11.6) | 163.0 (11.7) |

0.13.0 leads imgproxy by 7-13% here. The 2026-07 AWS run showed
1.17-1.34x on its JPEG column, so read that column as the pre-0.11
build. The AWS c7 run was redone on 0.15.0 in 2026-10
([below](#official-harness-on-real-aws-hardware-c7ilarge-and-c7glarge)).

**0.14.0** (2026-10-02). This adds the linear-light half-size luma
decode for 4:2:0 sources reduced about 3.8x or more, which this cell
is ([#60](https://github.com/oximg/oximg/issues/60)).

It was measured on an Intel i7-1360P, because the usual AMD Ryzen 7
8745HS machine was busy with unrelated builds that invalidated two runs
there. The harness shape is the same. The server is pinned to P-cores:
one SMT pair or two physical cores. k6 runs on E-cores 8-13 and nginx
on 14-15. The machine was on AC with the battery full. Configurations
rotated each round; all 30 cells had 0 failed checks.

| server | SMT pair, 2 VUs | SMT pair, 8 VUs | 2 cores, 2 VUs | 2 cores, 8 VUs |
|---|---|---|---|---|
| oximg 0.13.0 | 95.5 (20.4) | 86.8 (22.9) | 110.5 (16.3) | 114.5 (16.9) |
| **oximg 0.14.0** | **119.6** (16.2) | **113.3** (17.5) | **146.7** (12.5) | **140.2** (13.8) |
| imgproxy 4.0.17 | 97.8 (18.9) | 91.5 (21.3) | 119.4 (14.9) | 109.5 (17.4) |

0.14.0 leads imgproxy by 1.22-1.28x here and does 22-33% more req/s
than 0.13.0. Output averages 45.0 KB per image, against 45.6 KB for
0.13.0 and 42.6 KB for imgproxy. The two tables come from different
CPUs, so compare within a table, not across them.

**0.15.0** (2026-10-19), back on the Ryzen 7 8745HS. Its changes
for this cell are the linear-light shrink's pair-table lookup
([#84](https://github.com/oximg/oximg/pull/84)); this cell's outputs
are byte-identical to 0.14.0. Same shape as the 0.13.0 table: server
on one SMT pair or two physical cores, k6 and nginx on 7,15, rotated
order, 3 rounds at 2 VUs, all 18 cells with 0 failed checks. The
binary was f1c65b4, one commit before the 12,000-pixel sequential
threshold and before #77's raw YCbCr path. Neither applies here:
512-pixel outputs are above the threshold, and DIV2K at fit 512 takes
the half-size decode rather than the full-scale decode #77 speeds up.

| server | SMT pair, 2 VUs | 2 cores, 2 VUs |
|---|---|---|
| oximg 0.14.0 | 155.0 (12.6) | 194.9 (8.9) |
| **oximg 0.15.0** | **158.2** (12.3) | **197.6** (8.7) |
| imgproxy 4.0.17 | 120.9 (15.3) | 159.6 (11.0) |

0.15.0 leads imgproxy by 1.24-1.31x here and does 1.4-2.0% more req/s
than 0.14.0 at 1.8-2.0% less CPU per request. Output averages 44,994
bytes per image for both oximg releases and 42,571 for imgproxy.
Against the 0.13.0 table on the same CPU (2026-10-01), imgproxy is
within 5% and oximg is 1.16-1.28x faster.

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
their same-URL peak RSS low. Coalescing is **per-process**: a single
instance realises these numbers, while a horizontally scaled deployment
(Cloud Run, Lambda, an autoscaled Deployment behind a round-robin
Service) sees the benefit fall toward zero as instances multiply —
identical requests land on different processes and never meet
(measured in the field: 681 leaders, 0 followers across 6 Cloud Run
instances). No oximg-side setting changes this; a CDN in front does
the deduplication that matters there. Peak RSS under 16-way diverse load: oximg
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
— sequential (SOF1) vs progressive jpegli differ only in entropy layout).
Smooth synthetic noise is the content where oximg's supersampled
linear-light resize matters least; on the real-photo corpus the same
q80 comparison on 0.15.0 is **77.5 / 77.1 / 75.9 vs imgproxy 4.0.17's
71.2 / 65.8 / 56.2** for 768px/2000px/4000px sources, +6 to +20
points. imgproxy passes oximg's q80 score only at q90: 78.7 from
62.5 KB, against oximg's 77.5 from 34.2 KB. Full protocol and sweeps
in [bench/quality/QUALITY.md](bench/quality/QUALITY.md).

Both oximg rows are the auto overlap gate composing one pipeline:
decode fused with resize+encode on a second thread below saturation,
one core per request at saturation — serial and fused stream through
the same SIMD row kernel, so a URL's bytes never depend on load. The
speed profile is `OXIMG_JPEG_PROGRESSIVE=0` (sequential jpegli, SOF1: entropy
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

> Measured 2026-07 on a pre-0.11 build. The JPEG column overstates
> current releases; see [JPEG sources since 0.11](#jpeg-sources-since-011-2026-10).

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

The harness ran on the instance types imgproxy publishes its results
on, deployed with the harness's own CloudFormation template: Ubuntu
24.04, Docker, us-east-1, measured 2026-10-21 ([#63](https://github.com/oximg/oximg/issues/63)).
Every server ran from a published image or package:
- oximg 0.15.0 and 0.14.0 from Docker Hub
  (`oximg/oximg@sha256:8afd4b15…` and `@sha256:f787a37d…`);
- imgproxy 4.0.17 (`darthsim/imgproxy@sha256:3a7d7c5d…`);
- imagor 1.9.13;
- thumbor 7.8.0.

The harness was changed in three ways:
- [bench/image-servers-benchmark.patch](bench/image-servers-benchmark.patch)
  adds the oximg service;
- a `VUS` override of the Makefile's `-u $(nproc)`, used only for the
  8-VU rounds;
- thumbor's Dockerfile is pinned to `python:3.13-slim`. Upstream's
  `python:3-slim` is now Python 3.15, for which `pillow-avif-plugin`
  fails to build.

The full table: one 5-minute cell per server and format, at 2 VUs (the
harness default), with the server order rotated per format. Values are
req/s with p95 in parentheses, and every cell passed 100% of its
checks. Each non-JPEG cell is **a single run**, short of
[METHODOLOGY.md](bench/METHODOLOGY.md)'s interleaved rounds, so read a
gap there smaller than the ~3% instance-to-instance variance as a tie.
JPEG has its own rounds below.

c7i.large (Intel Xeon Platinum 8488C, 2 vCPU = one SMT core):

| Server | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| **oximg 0.15.0** | **88.5** (29 ms) | **34.6** (75 ms) | **32.5** (88 ms) | **16.5** (174 ms) |
| imgproxy 4.0.17 | 71.9 (38 ms) | 15.1 (176 ms) | 21.5 (129 ms) | 16.2 (179 ms) |
| imagor 1.9.13 | 63.7 (41 ms) | 16.3 (165 ms) | 18.6 (143 ms) | 10.7 (270 ms) |
| thumbor 7.8.0 | 53.2 (47 ms) | 9.1 (293 ms) | 14.5 (180 ms) | 12.5 (219 ms) |

c7g.large (Graviton3, Neoverse-V1, 2 physical cores):

| Server | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| **oximg 0.15.0** | **101.0** (25 ms) | **40.8** (63 ms) | **42.5** (68 ms) | **24.8** (119 ms) |
| imgproxy 4.0.17 | 65.7 (40 ms) | 20.9 (125 ms) | 25.4 (111 ms) | 19.9 (143 ms) |
| imagor 1.9.13 | 55.8 (45 ms) | 21.9 (117 ms) | 19.3 (133 ms) | 13.4 (209 ms) |
| thumbor 7.8.0 | 59.4 (43 ms) | 12.3 (212 ms) | 20.1 (130 ms) | 14.6 (198 ms) |

JPEG, the cell the 0.11-0.15 decode changes touched, was then run in 3
rounds per instance. Each round ran oximg 0.15.0, oximg 0.14.0 and
imgproxy in rotated order, at 2 and at 8 VUs. Values are the mean
req/s with the range over rounds, and every cell passed 100% of its
checks.

| | 0.15.0 | 0.14.0 | imgproxy 4.0.17 | 0.15.0 / imgproxy |
|---|---|---|---|---|
| c7i, 2 VUs | **89.3** (88.6-90.4) | 87.6 (87.3-88.1) | 72.0 (71.5-72.9) | **1.24x** |
| c7i, 8 VUs | **90.1** (89.4-90.6) | 88.1 (87.9-88.3) | 71.7 (71.5-72.0) | **1.26x** |
| c7g, 2 VUs | **100.3** (100.1-100.5) | 99.5 (99.3-99.8) | 65.7 (65.5-65.9) | **1.53x** |
| c7g, 8 VUs | **103.3** (103.1-103.7) | 102.4 (102.3-102.5) | 68.5 (68.1-68.9) | **1.51x** |

Notes:

- **#63's target was ≥1.3x imgproxy on both instances.** c7g meets it.
  On c7i the answer depends on the instance. This one measured
  1.24-1.26x. A second fresh c7i.large (2026-10-21, #107) ran the
  same images with the same cell method and measured oximg 110.0 /
  109.7 against imgproxy 82.6 / 82.2, i.e. **1.33x**. Both were Xeon
  Platinum 8488C, stepping 8, yet oximg was 23% faster and imgproxy
  14% faster on the second instance. Read c7i as **1.24-1.33x across
  instances**. The same run ruled out the suspects:
  - The one-SMT-core topology: a Zen 4 SMT pair with nginx and k6
    co-located measures 1.37-1.38x.
  - The Intel-only 512-bit horizontal pass: the same clock either
    way, and the 256-bit pass is within 0.6-1.2%.
- **Against 2026-07** (a pre-0.11 build, retired by this run), oximg's
  JPEG cell rose from 78.7 to 88.5 on c7i and from 91.2 to 101.0 on
  c7g. imgproxy moved from 67.0 to 71.9 and from 68.0 to 65.7. The
  other oximg cells rose 2-6%.
- **0.15.0 over 0.14.0 is +0.8-2.3% on JPEG**, from the pair-table
  shrink. Neither the 12,000-pixel sequential threshold nor #77's raw
  YCbCr path applies here: these outputs are 512 px, and DIV2K at fit
  512 takes the half-size decode.
- **AVIF on c7i is a tie**: 16.5 against 16.2 in single runs, inside
  the ~3% instance-to-instance variance that earlier same-run anchors
  bounded. The other non-JPEG leads are single runs too, but large
  ones: PNG 1.9-2.1x, WebP 1.5-1.7x, AVIF on c7g 1.25x.
- [bench/aws-harness-rounds.sh](bench/aws-harness-rounds.sh) is the
  loop that produced these tables. Run it on the instance, from the
  harness checkout. One deviation: the thumbor row ran after phase B,
  once its build was fixed and disk space freed (the template's 8 GB
  root disk filled up). In the script, thumbor runs in its place in
  the rotation.

## Newer instance generations (c8i.large and c9g.large)

> Measured 2026-07 on a pre-0.11 build; the same caveat as the c7
> tables applies to every JPEG-source cell.

The same harness and deployment on the newest compute generations
available in us-east-1 as of 2026-07 — c8i.large (Intel Xeon 6975P-C
"Granite Rapids", one SMT core at 3.9 GHz) and c9g.large (next-gen
Graviton, two physical cores at 2.8 GHz). us-east-1 offers no
c9i.large yet. The c7 tables above remain the canonical comparison
(imgproxy's published numbers are c7-based); this section is the
forward-looking data point. All four servers measured together per
instance, 100% checks; req/s (p95).

c8i.large:

| Server | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| oximg (defaults) | **110.3** (24 ms) | **44.7** (58 ms) | **40.7** (70 ms) | **21.5** (134 ms) |
| imgproxy | 90.3 (31 ms) | 19.0 (142 ms) | 27.0 (104 ms) | 20.8 (140 ms) |
| imagor 1.9.2 | 76.9 (34 ms) | 20.8 (130 ms) | 24.5 (110 ms) | 14.5 (199 ms) |
| thumbor 7.x | 66.4 (38 ms) | 11.2 (235 ms) | 18.6 (139 ms) | 16.0 (171 ms) |

c9g.large:

| Server | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| oximg (defaults) | **135.6** (19 ms) | **53.9** (48 ms) | **58.9** (51 ms) | **36.2** (82 ms) |
| imgproxy | 112.8 (25 ms) | 32.8 (80 ms) | 36.3 (79 ms) | 32.4 (90 ms) |
| imagor 1.9.2 | 100.6 (26 ms) | 34.5 (75 ms) | 29.7 (88 ms) | 22.3 (129 ms) |
| thumbor 7.x | 100.4 (26 ms) | 18.7 (140 ms) | 30.6 (86 ms) | 22.1 (135 ms) |

Cross-format cells (JPEG sources):

| JPEG→ | c8i oximg | c8i imgproxy | c9g oximg | c9g imgproxy |
|---|---|---|---|---|
| WebP | **89.6** (30 ms) | 46.7 (55 ms) | **116.6** (23 ms) | 56.8 (46 ms) |
| AVIF | **64.8** (40 ms) | 63.4 (43 ms) | **96.9** (27 ms) | 87.6 (33 ms) |

Notes:

- oximg leads every cell on both generations — including JPEG→AVIF on
  the Intel SMT topology (+2% on c8i at the preset-8 default), where
  c7i measured at parity: Granite Rapids narrows the SMT contention
  penalty that SVT-AV1's dense vector kernels pay on Sapphire Rapids.
  `OXIMG_AVIF_SPEED=9` applies on top for deployments that want a
  wider margin.
- Generational uplift for oximg at unchanged defaults: c7i → c8i
  +37-45% per cell; c7g → c9g +49-71% (JPEG→AVIF +71%, 56.5 → 96.9
  req/s — the new Graviton is disproportionately good at the SVT
  encode).

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

Since 0.3.0 the mozjpeg presets also fuse the decode with the resize
on a second thread under the same `OXIMG_OVERLAP` gate (the one-shot
mozjpeg encode runs after; bytes are identical to the serial path and
never depend on the gate). Interleaved A/B on the Ryzen SMT pair,
512-fit DIV2K, single-request medians: `fast` 8.0 → 7.2 ms (-10%),
`small` 24.4 → 23.8 ms (-2.6% — the trellis encode dominates its
request), with saturated 2-VU throughput unchanged (the auto gate
closes under load, as designed).

Quality per byte for each encoder is measured in
[bench/quality/QUALITY.md](bench/quality/QUALITY.md).

## Cold start

Container platforms (Cloud Run class) bill the whole cold-start chain:
image pull + container start + app ready + first real request.
[bench/coldstart.sh](bench/coldstart.sh) measures the app-controllable
part as distributions (the tail is what pages you), from process/
container start to a 200 on `/health` (ready) and to the first real
`/resize` 200 (first work). Ryzen 7 8745HS, local Docker (runc; gVisor
platforms add their own constant):

| | ready p50 / p95 | first work p50 / p95 |
|---|---|---|
| oximg native binary | **6 / 6 ms** | **13 / 14 ms** |
| oximg Docker | 124 / 192 ms | 132 / 199 ms |
| imgproxy Docker | 138 / 143 ms | 145 / 151 ms |

Under Docker both servers are dominated by the ~120ms container
runtime floor — the app-level difference shows in the native row:
oximg's own init is ~6ms (a single 8.9MB static-leaning binary, no
lazy init on the first request beyond ~1ms of LUT building). The
pull-and-provision side favors the same shape: the oximg image is
113MB vs imgproxy's 235MB, and idle RSS after ready is 10MB vs 29MB —
smaller instances, faster pulls, cheaper warm pools.

## Connection capacity and overload behavior

How many concurrent connections can one instance hold, and what does
overload look like? [bench/stress.sh](bench/stress.sh) ramps constant
open connections (k6 VUs, each pinned to its own distinct URL out of a
4100-URL space so request coalescing can never serve duplicates),
server and load generator on disjoint cpusets of the same Ryzen box
(server: 4 cores + SMT), 30s per level, 30s client timeout, DIV2K
512-fit JPEG. req/s, latency, failures, `memory.peak`:

| c | oximg | imgproxy |
|---|---|---|
| 16 | 649 rps, p50 24ms, p99 31ms, **0%** fail, 50MB | 444 rps, p50 36ms, p99 48ms, 0% fail, 128MB |
| 256 | 598 rps, p50 0.43s, p99 0.44s, **0%**, 62MB | 443 rps, p50 0.58s, p99 0.64s, 0%, 167MB |
| 1024 | 587 rps, p50 1.7s, p99 1.8s, **0%**, 88MB | 439 rps, p50 2.3s, p99 2.4s, 0%, 248MB |
| 2048 | 578 rps, p50 3.5s, p99 4.2s, **0%**, 106MB | 439 rps, p50 4.6s, p99 4.7s, 0%, 358MB |
| 4096 | 565 rps, p50 7.2s, p99 8.9s, **0%**, 168MB | 489 rps, p50 4.8s, **p95 30s, 12% fail**, 361MB |
| 8192* | 0% fail, 265MB | **29% fail**, 356MB |

\* c=8192 exceeds the 4100 distinct URLs, so oximg's coalescing
engages and its throughput is no longer comparable; the failure and
memory columns remain valid.

Two different failure philosophies show up. oximg accepts every
connection and queues work behind the CPU semaphore: throughput
degrades only 13% from c=16 to c=4096, latency grows linearly with
queue depth (pure queueing, p99 ≈ p50 — the FIFO semaphore is fair),
memory stays at ~25-30KB per open connection, and nothing fails
through 8192 connections. imgproxy holds steady to c=2048 and then
starts starving connections (its default `IMGPROXY_MAX_CLIENTS`
ceiling is 2048): p50 stays low while p95 pins at the client timeout —
a bimodal split where some clients never get served — reaching 12%
failures at 4096 and 29% at 8192.

Neither server applies backpressure by default; if you want load
shedding rather than queueing, put a queue-depth limit in front. What
this table establishes is the safe envelope: on 4 cores, oximg
sustains its full throughput with zero failures at any connection
count a real deployment will see, and degrades by latency only.

## Metadata sources (orientation / ICC)

The dataset above carries no orientation tags or ICC profiles. Real
traffic does — phone photos are almost always EXIF-oriented, and
print/design assets carry ICC profiles — so these cells measure the
same harness over variants of the DIV2K set with an orientation-6 EXIF
tag or a real sRGB profile (sRGB2014.icc) spliced into each JPEG (byte
surgery, no recompression; see
[bench/metadata_cells.sh](bench/metadata_cells.sh)). Ryzen 7 8745HS,
`cpuset: "0,1"`, 2 VUs, oximg 0.4.4 vs imgproxy at its defaults, two
interleaved rounds each (both shown; averaged for the delta).

req/s, oximg vs imgproxy:

| Cell | oximg (r1, r2) | imgproxy (r1, r2) | oximg lead |
|---|---|---|---|
| clean JPEG→JPEG (baseline) | 194.0, 200.5 | 163.3, 162.7 | **+21%** |
| oriented JPEG→JPEG | 190.1, 197.5 | 156.4, 157.8 | **+23%** |
| profiled JPEG→JPEG | 195.3, 199.7 | 130.8, 130.6 | **+51%** |
| oriented JPEG→AVIF | 115.1, 115.8 | 101.6, 100.6 | **+14%** |
| profiled JPEG→AVIF | 119.3, 117.4 | 92.1, 91.6 | **+29%** |

- oximg's own metadata cost is in the noise: orientation is a
  post-resize rotation on the small output frame (a channel-
  monomorphized pass, ~0.1 ms) and ICC is a byte-for-byte
  pass-through (no pixel work), so its cells sit within a round of the
  clean baseline.
- imgproxy's lead *widens* on metadata sources because it does more
  work: it applies the ICC profile — a full color transform to sRGB —
  and then strips it, which on this real profile costs ~20% of its
  JPEG throughput (163 → 131) and clips every out-of-gamut color in
  the process. oximg passes the profile through untouched, so a
  wide-gamut source stays wide-gamut *and* costs nothing (see
  [quality/QUALITY.md](bench/quality/QUALITY.md) on the fidelity
  difference). The orientation gap is smaller but same-signed:
  imgproxy's saturated oriented-JPEG cell drops ~3.4% under its clean
  baseline, oximg's ~1.5%.
- These are single-box numbers (not the AWS grid); they measure the
  *relative* metadata cost, which is what a real deployment's
  phone-photo / design-asset traffic pays.

## Ruby: as a library, against the image-processing gems

Everything above measures oximg as a server, against servers. This
section measures it as a **library**, through the [`oximg`
gem](rubygem/oximg), against what a Rails app would otherwise call:
`ruby-vips` (ActiveStorage's variant processor since Rails 7),
`image_processing` (the layer ActiveStorage actually calls, on both its
backends), and `mini_magick` (366M downloads, the older default).

Task: the canonical ActiveStorage variant — fit within 750x750, never
enlarging, re-encoded as JPEG at quality 80 — each gem at its own
defaults, because defaults are what an app gets. Best of 3 per image,
averaged across the group. oximg 0.10.1, ruby-vips 2.3.0,
image_processing 1.14.0, mini_magick 5.3.3, pinned by one
`Gemfile.lock` on all three machines. Harness:
[bench/ruby/bench.rb](bench/ruby/bench.rb).

Peak RSS is why each (group, gem) pair runs in its own subprocess under
`/usr/bin/time`: a single Ruby VM that has loaded libvips, loaded
ImageMagick and spawned oximg reports the high-water mark of whichever
ran worst.

### Large source: 4000x2667 JPEG → 750x500 (n=3)

wall ms / CPU s / peak RSS:

| Gem | Apple M2 Max | AMD Ryzen 7 8745HS | Intel i7-1360P |
|---|---|---|---|
| **oximg** | 73.4 / **0.83** / **37.5 MB** | 76.6 / 0.84 / **41.1 MB** | 76.2 / **0.84** / **41.0 MB** |
| ruby-vips | 74.0 / 0.91 / 373.3 MB | **71.7** / **0.83** / 419.3 MB | 78.5 / 0.88 / 424.4 MB |
| image_processing/vips | **73.1** / 0.95 / 156.5 MB | 72.9 / 0.92 / 365.1 MB | **76.0** / 0.97 / 317.1 MB |
| image_processing/magick | 317.9 / 2.97 / 181.2 MB | 235.9 / 2.96 / 169.9 MB | 244.7 / 3.14 / 169.9 MB |
| mini_magick | 316.3 / 2.97 / 182.2 MB | 235.9 / 2.96 / 170.0 MB | 240.0 / 3.10 / 169.9 MB |

### Smaller sources, wall ms (M2 Max / Ryzen / Intel)

| Gem | 2000x1334 (n=3) | 768x512 (n=8) |
|---|---|---|
| oximg | 23.0 / 20.6 / 21.0 | 12.8 / 12.0 / 15.8 |
| ruby-vips | **18.5** / **18.4** / **21.6** | **6.3** / **5.7** / **9.8** |
| image_processing/vips | 20.3 / 21.6 / 25.6 | 7.7 / 7.8 / 11.7 |
| image_processing/magick | 79.7 / 69.8 / 67.8 | 27.2 / 26.5 / 26.1 |
| mini_magick | 79.8 / 68.9 / 67.5 | 27.2 / 26.7 / 26.7 |

Peak RSS on those groups, range across the three CPUs: oximg
**22.3-30.0 MB** / **22.5-31.5 MB**, ruby-vips 94.0-145.9 / 76.4-93.7,
image_processing/vips 84.7-149.7 / 70.8-94.5, mini_magick 51.8-62.2 /
28.8-34.5.

### Baseline: gem loaded, nothing processed

| Gem | M2 Max | Ryzen | Intel |
|---|---|---|---|
| oximg | **28.7 MB** | **22.2 MB** | **23.5 MB** |
| mini_magick | 30.0 MB | 23.8 MB | 25.3 MB |
| ruby-vips | 47.3 MB | 51.8 MB | 52.3 MB |
| image_processing/vips | 48.5 MB | 56.9 MB | 57.3 MB |

Requiring libvips costs 25-35 MB resident in *every* process that loads
it, before an image is touched — multiplied by the worker count. The
oximg gem keeps the codecs in a subprocess, so the web process carries
almost nothing.

### Output size and quality

Output bytes were identical on all three machines, so this table is
per-group. SSIMULACRA2 is from the macOS run (the Linux boxes have no
`ssimulacra2` package), scored against a linear-light Lanczos reference
at the same dimensions, per
[bench/quality/QUALITY.md](bench/quality/QUALITY.md):

| Gem | large KB / ssim2 | medium KB / ssim2 | kodak KB / ssim2 |
|---|---|---|---|
| oximg | **84.4** / 59.90 | **84.6** / 70.67 | **73.1** / 78.07 |
| ruby-vips | 86.6 / 62.06 | 87.5 / 66.40 | 78.1 / 75.82 |
| image_processing/vips | 97.9 / 67.34 | 99.0 / 70.07 | 86.8 / 70.57 |
| mini_magick | 86.4 / 64.11 | 86.5 / 67.09 | 89.7 / 78.64 |

**The ssim2 column is not a verdict.** At one quality setting the
contenders write different numbers of bytes — image_processing/vips
writes 16% more than oximg on the large group — so a single point
cannot separate encoder quality from encoder size. Ranking these
encoders needs the iso-byte sweep that QUALITY.md runs for the server;
what this table does establish is that oximg writes the smallest output
in every group.

### What the numbers say

- **Memory is the difference, and it holds on all three CPUs.** On
  4000x2667 sources oximg peaks at 37-41 MB against ruby-vips'
  373-424 MB — about 10x — while `image_processing/vips` lands anywhere
  between 157 and 365 MB depending on the platform. For a deployment
  with a container memory limit, that spread is its own problem: oximg's
  own figure varies ±5% across the three.
- **Time is a tie with libvips.** Every oximg/ruby-vips pair on the
  large group is within 6%, and oximg's CPU seconds are equal or lower.
- **ImageMagick burns 3.5-4x the CPU** everywhere, and is 3-4x slower in
  wall time. It is also the one path that is markedly worse on Apple
  silicon (318 ms vs 235-245 ms on x86).
- **Small sources are oximg's weak spot**: one process spawn per image
  costs a median 4.2 ms, which dominates a 6 ms job. That is a property
  of driving a CLI, not of the pipeline, and an in-process native
  extension would recover it without changing the gem's API.

Caveats: within a machine the comparison is exact — every contender ran
the same way, from the same lockfile. Across machines it carries the
system Ruby patch level (3.4.1 / 3.4.10 / 3.4.8) and each
distribution's own libvips and ImageMagick builds. Laptop numbers are
only valid on a charged battery: the Intel box measured 28-38% slow at
1% battery even on AC.

These rows ran against the released `oximg` gem 0.10.1, which bundles a
binary from before the decode-scale change — so the oximg row is what
`gem install oximg` gives you today, and the next release will move it
along the frontier above: slower on the large group, and scoring where
QUALITY.md's new numbers put it. The memory column is unaffected, which
is the column this comparison turns on.

## Decode scale: the throughput/quality frontier (2026-08)

Every throughput table above was measured while JPEG shrink-on-load was
the default. It no longer is (see the `OXIMG_DCT_MARGIN` row in the
README): it only ever cost quality, and the more of the reduction it
did, the more it cost. That moves the large-source cells, and the
honest way to show it is not one number but the frontier — because the
old default's throughput was bought at a quality nobody would choose on
purpose.

Same box (Apple M2 Max), 7360x4912 → 500x500, `ab -n 300 -c 8`, three
interleaved rounds. The quality column is the same reduction (14.7x) on
real photographs — SSIMULACRA2 against a linear-light Lanczos reference,
6 sources, q80 — since plasma fractals say nothing about quality:

| Decode | `OXIMG_DCT_MARGIN` | req/s | SSIMULACRA2 |
|---|---|---|---|
| full size (**default**) | unset | 41.3 | **76.9** |
| DCT does ≤ 2x | ~7 | 48.4 | 75.7 |
| DCT does ≤ 4x | ~3.5 | **62.7** | 71.5 |
| DCT does the lot (old default) | 1.7 | **71.4** | 59.4 |
| *imgproxy 4.0.9, for reference* | — | *60.7* | *~56* |

Read the last two rows together: the 71.7 req/s this document used to
lead with on large sources was a 59.4-point output, 17.5 points below
what the same pipeline produces from a full decode. The default now
sits at the top of the quality column and pays 42% of the throughput
for it.

Deployments that would rather have the throughput can buy it back by
the rung, and the middle rung is the interesting one: at
`OXIMG_DCT_MARGIN=3.5` oximg does 62.7 req/s at 71.5, which is ahead of
imgproxy on **both** axes at once. That is a better claim than the old
one, and it is the one the numbers actually support.

What this does *not* change: the peak-RSS rows. Shrink-on-load never
bought memory on the streaming JPEG path — nothing full-frame is
resident there — and the buffered paths (CMYK/YCCK JPEG, WebP) keep
their 1.7 margin precisely because it is memory they buy. Measured
across the change: CMYK 23.7 MB both ways, WebP 21.8 → 21.4 MB.

## Notes

- **Every throughput table below this line predates the decode-scale
  change** and was measured with shrink-on-load on, i.e. at the bottom
  row of the frontier above. The oximg cells for large sources are the
  ones that move; small-source cells (where no shrink was selected
  anyway) do not, and no competitor cell does. Sized on the one cell
  that could be re-run locally: 71.4 → 41.3 req/s at 7360x4912 → 500,
  614.97 → 513.08 at 2000x1333 → 500. The AWS grids cannot be re-run
  from here, so they are left as measured and labelled rather than
  quietly adjusted.
- Measurement provenance: the official-harness tables (local Ryzen and
  AWS) were measured at the 0.3.0 cross-format + fused-overlap state
  (2026-07-05). The metadata work since (0.4.x: EXIF/AVIF orientation,
  ICC pass-through, animated first-frame) is byte-transparent for
  metadata-free sources — the benchmark dataset carries no orientation
  tags or ICC profiles (verified) and its output is byte-identical
  across 0.3.0→0.4.x (18/18 URL hash matrix) — so these throughput
  cells are unchanged by it. Sources that *do* carry metadata are
  measured separately in "Metadata sources" below. The oximg rows were
  re-measured after each significant pipeline change, and competitor
  rows whenever the environment changes (same-box anchors bound
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
# cold start (needs target/release/oximg + the two Docker images):
bench/coldstart.sh
# connection-capacity ramp (needs the harness dataset and k6's image):
DATASET=~/benchmark/dataset bench/stress.sh
# Ruby gems (needs the quality corpus, libvips and ImageMagick; the
# oximg gem resolves target/debug/oximg, or one on PATH):
cd bench/ruby && bundle install && bundle exec ruby bench.rb 3
```

Docker (Linux): build with the repo `Dockerfile`; run competitors from
their official images with the same `images/` volume; drive load with
`ab` (e.g. from `httpd:2.4-alpine` with `--network host`); read
`/sys/fs/cgroup/memory.peak` and `cpu.stat` inside each container.
