# Output quality benchmark (SSIMULACRA2)

Output quality of oximg, imgproxy, and sharp at identical settings,
scored with [SSIMULACRA2](https://github.com/cloudinary/ssimulacra2)
(30 = low, 50 = medium, 70 = high, 90 ≈ visually lossless; differences
above ~2 points are generally considered perceptible).

## Method

Two test groups:

- **Group A (encoder isolation)**: identical post-resize RGB pixels fed
  to each JPEG encoder, scored against the pre-encode pixels — measures
  encoder quality-per-byte only.
- **Group B (end-to-end)**: each service resizes and encodes from the
  same JPEG source, scored against a linear-light Lanczos reference
  (`magick -colorspace RGB -filter Lanczos -resize ... -colorspace sRGB`).

Corpus: the 24 Kodak images (converted to q97 4:4:4 JPEG as the common
source, 768x512), 3 real photographs at 4000x2667 (pinned picsum IDs),
and 2000px versions of the same 3. All fit into 500x500. Quality sweep:
60/70/75/80/85/90.

Contenders: oximg (this repo), imgproxy (Homebrew 4.0.9 and Docker
v4.0.11 produce identical scores) with per-URL `quality:N`, imagor
v1.9.2 (Docker), sharp 0.34 with bundled libvips (`mozjpeg:false/true`),
and ImageMagick's plain libjpeg-turbo encode.

## Group A — encoder isolation

Bytes needed to reach a given score (geometric mean across images),
relative to plain libjpeg-turbo:

| Encoder | S=70 | S=80 |
|---|---|---|
| oximg default (jpegli, progressive) | **-11.0%** | **-12.4%** |
| oximg `PRESET=small` (mozjpeg trellis+progressive) | -12.7% | -10.0% |
| sharp `mozjpeg:true` | -12.8% | -10.1% |
| oximg `PRESET=fast` (mozjpeg fastest + optimized Huffman) | +0.1% | ~0% |
| sharp default | -0.8% | -2.0% |
| libjpeg-turbo (imgproxy's encoder) | baseline | baseline |

At matched bytes-per-quality the jpegli encoder runs at roughly half the
CPU of the mozjpeg trellis path (`PRESET=small`).

## Group B — end-to-end (q80, scored vs linear-light reference)

| Source | oximg (defaults, jpegli) | oximg `PRESET=fast` | imgproxy default | imgproxy linear* | imagor 1.9.2 |
|---|---|---|---|---|---|
| Kodak 768px | **77.5** | 75.9 | 71.2 | 72.4 | 71.2 |
| medium 2000px | **72.2** | 72.6 | 60.1 | 69.4 | 60.1 |
| large 4000px | **67.5** | 68.3 | 49.7 | 49.7* | 51.6 |

\* imgproxy with `IMGPROXY_USE_LINEAR_COLORSPACE=1`. On the large-source
group its output was byte-identical to the default configuration.

At q80 oximg produces 33.9 KB (Kodak group mean, jpegli default);
imgproxy reaches a lower score (76.0) at q90 with 63.8 KB.

The speed profile (`OXIMG_JPEG_PROGRESSIVE=0`, see BENCH.md) scores
**identically to the default on all 30 corpus images** — baseline and
progressive jpegli encode the same quantized coefficients, differing
only in entropy layout — at +9-11% bytes (Kodak group: 37.2 KB vs
34.0 KB; imgproxy produces 35.0 KB at 71.2). Re-measured 2026-07 at
HEAD (streamed SIMD resize on both architectures): 77.5 / 72.1 / 67.3
for the three groups, matching the table above.

Scores with oximg's quality-reducing knobs (`OXIMG_RESIZE=srgb
OXIMG_DCT_MARGIN=1.0`) are ~60 on the medium group — the same level as
imgproxy's and imagor's defaults. Throughput for all profiles is in
[../../BENCH.md](../../BENCH.md).

## PNG and WebP (same-format in/out, fit 500x500)

Kodak sources (PNG originals, all 24 for the PNG row; WebP encoded at
q90), scored against a linear-light Lanczos reference of the same
source, local Apple M2 Max, imgproxy 4.0.9. PNG output is lossless, so
its score isolates pure resize quality.

| Format | Server | SSIM2 (linear ref) | avg size |
|---|---|---|---|
| PNG | oximg | **97.6** | 307.8 KB |
| PNG | imgproxy | 81.9 | 308.8 KB |
| WebP (q75) | oximg | **71.8** | **30.5 KB** |
| WebP (q75) | imgproxy | 61.7 | 33.1 KB |

The PNG row reflects the aarch64 NEON resize kernel, which carries f32
intermediate rows between the convolution passes; the earlier
fast_image_resize backend (u16-quantized intermediate) measures 95.2 on
the same corpus and remains available via OXIMG_RESIZE_BACKEND=fir.
x86-64 uses pic-scale, separately verified at equal quality.

WebP note: imgproxy resizes with libwebp's built-in scaler, which is
the source of its score; oximg decodes with quality headroom and
resizes in linear light. Throughput and latency for these formats are
measured under sustained load in [../../BENCH.md](../../BENCH.md).

## AVIF (same-format in/out, fit 512x512, Ryzen harness outputs)

First 10 DIV2K images of the harness dataset (AVIF sources produced by
the harness itself with vips at Q=65), served by each Docker contender
exactly as in the throughput run, scored against a linear-light Lanczos
reference computed from the decoded source. Mean SSIM2 / total bytes
for the 10 outputs:

| Server | SSIM2 (linear ref) | total bytes |
|---|---|---|
| oximg `OXIMG_AVIF_QUALITY=65` | **79.6** | 409 KB |
| oximg (default, q55) | **74.2** | **307 KB** |
| thumbor 7.x (q65) | 68.5 | 317 KB |
| imagor 1.9.2 (q65) | 68.4 | 306 KB |
| imgproxy (q65) | 67.5 | 319 KB |

The same nominal quality lands on very different rate/distortion
points: oximg encodes 10-bit 4:2:0 with SVT-AV1 tune=ssim. Its default
(q55) was chosen by operating point — smaller files than imgproxy's
q65 default at +6.7 SSIM2; matching nominal q65 instead trades +28%
bytes for +12.1.

## Notes

- The linear-light reference is produced by ImageMagick (also a
  Lanczos-family resampler). Group A is filter-independent; Group B
  relative ordering is unaffected since all contenders use lanczos3.
- All outputs were verified to have identical pixel dimensions before
  scoring.

## Reproduce

```sh
cargo build --release --example qcli
cd bench/quality && npm i
# corpus: Kodak PNGs from r0k.us + pinned picsum IDs; see run.py
IMGPROXY_BIND=:8082 IMGPROXY_LOCAL_FILESYSTEM_ROOT=$PWD/corpus imgproxy &
python3 run.py /tmp/qwork && python3 analyze.py /tmp/qwork/results.csv
```
