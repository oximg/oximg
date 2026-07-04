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

oximg release build vs imgproxy 4.0.9 (Homebrew) + vips 8.18.3, both at
quality 80, identical output dimensions, `ab`, 20-request warm-up,
servers restarted per scenario. Methodology after
[the imgproxy benchmark gist](https://gist.github.com/DarthSim/9d971d2859f3714a29cf8ce094b3fc55).

### Large: 7360x4912 (10.6MB) → 500x500, N=400, c=8

| Server | req/s | p50 | p95 | peak RSS | output |
|---|---|---|---|---|---|
| oximg (defaults) | **71.7** | 109 ms | 116 ms | — | 23.9 KB |
| oximg speed mode | 72.2 | 107 ms | 111 ms | **130 MB** | 23.9 KB |
| imgproxy | 60.7 | 127 ms | 138 ms | 317 MB | 22.9 KB |

### Medium: 2000x1333 (0.8MB) → 500x500, N=1000

| Server | c=8 req/s | c=12 req/s | peak RSS | output |
|---|---|---|---|---|
| oximg (defaults) | 533 | **647** | — | 23.4 KB |
| oximg speed mode | **799** | — | 32 MB | 23.5 KB |
| imgproxy | 590 | 615 | 124 MB | 22.4 KB |
| oximg `PRESET=small` | 395 | — | 42 MB | **18.2 KB** |

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
| oximg (defaults) | **190.8** (93 ms) | **72.7** (240 ms) | **70.6** (247 ms) | **33.7** (507 ms) |
| imgproxy | 155.8 (121 ms) | 30.5 (617 ms) | 46.0 (412 ms) | 33.4 (545 ms) |
| imagor 1.9.2 | 143.1 (169 ms) | 35.8 (670 ms) | 44.6 (493 ms) | 24.5 (920 ms) |
| thumbor 7.x | 106.8 (188 ms) | 18.4 (1150 ms) | 33.7 (616 ms) | 30.8 (646 ms) |

The relative order of the other three matches imgproxy's published
c7i.large results. PNG output at these settings measures 448KB per
image vs libvips' 482KB default. Output quality is measured in
[bench/quality/QUALITY.md](bench/quality/QUALITY.md).

AVIF (oximg built with `--features avif`, SVT-AV1 4.1 encode / dav1d
decode) is decode-bound for every server, which compresses the
throughput spread. Output operating points differ substantially at the
same nominal q65, so the quality table in QUALITY.md is the other half
of this cell: at default settings oximg spends 28% more bytes and
scores +12.1 SSIMULACRA2 over imgproxy; with `OXIMG_AVIF_QUALITY=55`
its output is smaller than imgproxy's and still scores +6.7 while
serving 34.3 req/s (p95 499 ms).

### AVIF with alpha

The DIV2K dataset has no alpha, so this variant re-encodes the same 99
sources with a synthetic alpha ramp (`avifenc -s 8 -q 65`) and runs the
identical 512x512-fit AVIF-out cell. oximg carries alpha as a second
SVT-AV1 auxiliary-image encode plus a second dav1d decode:

| Server | req/s (p95) |
|---|---|
| oximg (defaults) | **29.5** (578 ms) |
| thumbor 7.x | 27.3 (738 ms) |
| imagor 1.9.2 | 27.2 (720 ms) |
| imgproxy | 26.2 (684 ms) |

All runs 100% successful checks; every server's output carries the
alpha item (verified with avifdec).

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
| oximg (defaults) | **78.3** (32 ms) | **35.7** (72 ms) | **39.6** (72 ms) | 16.8 (163 ms) |
| imgproxy | 68.4 (39 ms) | 21.0 (123 ms) | 25.4 (111 ms) | 20.1 (141 ms) |
| imagor 1.9.2 | 57.7 (44 ms) | 22.0 (116 ms) | 19.7 (132 ms) | 13.7 (208 ms) |
| thumbor 7.x | 63.3 (41 ms) | 12.4 (209 ms) | 20.5 (128 ms) | 14.8 (195 ms) |

Notes:

- The c7i AVIF cell reflects the current defaults (two dav1d worker
  threads, quality 55); the earlier one-thread/q65 build measured 13.9.
- The c7g oximg cells were re-run after the aarch64 NEON resize kernel
  landed (resize stage 37 -> 20 ms on the full-decode shape); the
  imgproxy AVIF cell re-run on the same instance (20.1 vs 20.4) anchors
  comparability with the other contenders' original runs.
- The remaining c7g AVIF gap is decode-stage-bound. dav1d's in-frame
  threading works on Graviton3 (1.9x on two cores, verified with a
  minimal repro against dav1d 1.4.1/1.5.1/1.5.3); the stage's cost is
  split between the AV1 decode proper (~28 ms threaded) and the
  YUV-to-RGB conversion (~22 ms, currently scalar).

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

- Throughput tables above were measured with `PRESET=fast` as the
  encoder, before jpegli became the default; the preset table shows the
  relative cost of the current default.
- oximg defaults resize in linear light with 1.7x DCT decode headroom;
  speed mode (`OXIMG_RESIZE=srgb OXIMG_DCT_MARGIN=1.0`) matches the
  competitors' processing approach. Output quality for both settings is
  quantified in [bench/quality/QUALITY.md](bench/quality/QUALITY.md).
- The plasma-fractal test images compress differently from real photos;
  both servers consume the same files, so relative values hold. The
  quality benchmark uses Kodak and real photographs.
- imgproxy is a full-featured product (many formats, URL signing, remote
  sources, watermarks); oximg implements the JPEG resize path only.

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
