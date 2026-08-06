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
source, 768x512), 12 real photographs at 4000x2667 (pinned picsum IDs),
and 2000px versions of the same 12 (`magick large -resize 2000x1334
-quality 95`). All fit into 500x500. Quality sweep: 60/70/75/80/85/90.

The photograph groups were 3 images each until 2026-08, and the medium
group was not walked by `run.py` at all — its published row could not
be reproduced by the harness that claimed it. Both are fixed, so the
numbers below differ from earlier revisions for that reason as well as
the decode-scale change described under Group B.

Contenders: oximg (this repo), imgproxy (Homebrew 4.0.9 and Docker
v4.0.11 produce identical scores) with per-URL `quality:N`, imagor
v1.9.2 (Docker), sharp 0.34 with bundled libvips (`mozjpeg:false/true`),
and ImageMagick's plain libjpeg-turbo encode.

## Group A — encoder isolation

Bytes needed to reach a given score (geometric mean across images),
relative to plain libjpeg-turbo:

| Encoder | S=70 | S=80 |
|---|---|---|
| oximg default (jpegli, progressive) | **-10.0%** | **-12.5%** |
| oximg `PRESET=small` (mozjpeg trellis+progressive) | -11.9% | -9.7% |
| sharp `mozjpeg:true` | -11.9% | -9.8% |
| oximg `PRESET=fast` (mozjpeg fastest + optimized Huffman) | -0.8% | -2.0% |
| sharp default | -0.8% | -2.0% |
| libjpeg-turbo (imgproxy's encoder) | baseline | baseline |

At matched bytes-per-quality the jpegli encoder runs at roughly half the
CPU of the mozjpeg trellis path (`PRESET=small`).

## Group B — end-to-end (q80, scored vs linear-light reference)

| Source | oximg (defaults, jpegli) | oximg `PRESET=fast` | imgproxy default | sharp default | imagor 1.9.2* |
|---|---|---|---|---|---|
| Kodak 768px (n=24) | **77.5** | 76.0 | 71.2 | 71.2 | 71.2 |
| medium 2000px (n=12) | **77.5** | 76.4 | 65.8 | 65.8 | 60.1 |
| large 4000px (n=12) | **77.3** | 76.1 | 56.2 | 58.0 | 51.6 |

\* imagor is not driven by `run.py`; its column is carried over from the
earlier 3-image run and is on the old corpus.

**Shrink-on-load was the ceiling here, not the encoder.** These rows
were 77.5 / 74.7 / 71.4 until 2026-08, when JPEG shrink-on-load stopped
being the default (see the `OXIMG_DCT_MARGIN` row in the README):
decoding at full size and handing the whole reduction to the resampler
is worth +2.8 points on the medium group and +5.9 on the large one, for
the same bytes. Measured by running this harness twice over the same
corpus, once with `OXIMG_DCT_MARGIN=1.7` — every competitor's score is
identical across the two runs, which is the control that makes the
oximg delta attributable.

Note what the shape of the new column says: oximg now scores ~77.4 on
all three groups, where before it fell away as sources got larger. The
resize is no longer measurably lossy against the linear-light ground
truth at any ratio in the corpus, so what is left is the encoder — the
Group A number.

At q80 oximg produces 33.9 KB (Kodak group mean, jpegli default);
imgproxy reaches a lower score (76.0) at q90 with 63.8 KB.

The speed profile (`OXIMG_JPEG_PROGRESSIVE=0`, see BENCH.md) scores
**identically to the default on all 30 corpus images** — baseline and
progressive jpegli encode the same quantized coefficients, differing
only in entropy layout — at +9-11% bytes (Kodak group: 37.2 KB vs
34.0 KB; imgproxy produces 35.0 KB at 71.2). That comparison was made
on the 30-image corpus and at the old decode default; the property it
rests on (identical coefficients, different entropy layout) is not one
the decode scale can touch, but the byte figures are from that run.

Scores with oximg's quality-reducing knobs (`OXIMG_RESIZE=srgb
OXIMG_DCT_MARGIN=1.0`) are ~60 on the medium group — the same level as
imgproxy's and imagor's defaults. Both are explicit settings, so that
figure is unaffected by the default change; it is now the *distance*
from the default that grew, since the default no longer shrinks on
load. Throughput for all profiles is in
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

## Cross-format operating points (JPEG source → WebP / AVIF output)

Sweep over the Kodak corpus (fit 500x500, scored vs the same
linear-light reference as Group B; `xfmt_sweep.py`). Candidates are
decoded back to PNG through oximg itself (a same-dimension transcode
is a lossless passthrough). oximg's JPEG-out default is the anchor:

| Output (quality) | mean SSIM2 | mean bytes |
|---|---|---|
| JPEG q80 (default, anchor) | 77.5 | 34.8 KB |
| AVIF q45 | 65.1 | 17.7 KB |
| AVIF q50 | 69.7 | 20.9 KB |
| AVIF q55 (default) | **74.5** | **25.3 KB** |
| AVIF q60 | 78.2 | 30.2 KB |
| AVIF q65 | 80.3 | 33.6 KB |
| WebP q60 | 64.0 | 21.1 KB |
| WebP q70 | 67.9 | 23.8 KB |
| WebP q75 (default) | 69.5 | 25.2 KB |
| WebP q80 | 74.2 | 30.4 KB |
| WebP q85 | 78.5 | 36.9 KB |

Conclusions the defaults rest on:

- **AVIF keeps its same-format default (q55)** for cross-format: -27%
  bytes vs the JPEG default at -3.0 SSIM2, and one notch up (q60)
  strictly dominates JPEG q80 — fewer bytes *and* a higher score — for
  operators who want parity instead of savings.
- **AVIF dominates WebP across the whole curve**: at equal ~25 KB,
  AVIF q55 scores +5.0 over WebP q75. When negotiating
  (`OXIMG_AUTO_FORMAT`), prefer `avif,webp` order.
- **WebP keeps q75** because the official throughput harness pins all
  contenders to WebP q75, and a default change would decouple our
  same-format WebP cell from those tables. For JPEG→WebP conversions
  where quality matters more than bytes, `OXIMG_WEBP_QUALITY=80` is
  the better point (-13% bytes vs JPEG q80 at -3.3 SSIM2).
- **`OXIMG_AVIF_SPEED=9`** (SVT preset; default 8) is the AVIF
  throughput knob: at q55 it costs -0.6 SSIM2 at unchanged bytes
  (73.9/25.2 KB vs 74.5/25.3 KB on this sweep — still far above
  imgproxy's q65 at 67.5) and cuts the SVT encode ~28%, measuring +21%
  req/s on the JPEG→AVIF cell on an SMT-pair topology and +19% on a
  real c7i.large (53.3 vs 44.8 req/s, ahead of the same-run imgproxy
  anchor by +16% — BENCH.md). The default stays 8: quality per byte is
  the shipped identity, and the benchmarked cells are measured there.

## Metadata (orientation, ICC) — quality-neutral by construction

The 0.4.x metadata handling does not move any score here, and does not
need its own measured table:

- **Orientation** (EXIF, AVIF irot/imir) rotates the already-resized
  pixels; because Lanczos is separable, resize-then-rotate is exactly
  rotate-then-resize, so a corrected image scores identically to the
  same scene shot upright. The scored corpus carries no orientation
  tags, so Group A/B numbers are unaffected.
- **ICC pass-through** copies the profile bytes unchanged and never
  color-converts the pixels — SSIMULACRA2 (which scores in a fixed
  working space) sees identical pixels with or without it. The
  quality *difference* it makes is on wide-gamut sources at display
  time, not in this metric: the common proxy default normalizes to
  sRGB and strips the profile, permanently clipping out-of-gamut
  colors (a Display P3 photo loses its saturated reds/greens);
  pass-through preserves them. That is a fidelity property, verified
  by round-trip byte-equality of the profile rather than a score.

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
# cross-format sweep (needs --features avif):
cargo build --release --example qcli --features avif
python3 xfmt_sweep.py /tmp/xfmt
```
