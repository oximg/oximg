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
| oximg `PRESET=small` (mozjpeg trellis+progressive) | **-12.7%** | **-10.0%** |
| sharp `mozjpeg:true` | -12.8% | -10.1% |
| oximg default (fast) | +0.1% | ~0% |
| sharp default | -0.8% | -2.0% |
| libjpeg-turbo (imgproxy's encoder) | baseline | baseline |

## Group B — end-to-end (q80, scored vs linear-light reference)

| Source | oximg (defaults) | imgproxy default | imgproxy linear* | imagor 1.9.2 |
|---|---|---|---|---|
| Kodak 768px | **75.9** | 71.2 | 72.4 | 71.2 |
| medium 2000px | **72.6** | 60.1 | 69.4 | 60.1 |
| large 4000px | **68.3** | 49.7 | 49.7* | 51.6 |

\* imgproxy with `IMGPROXY_USE_LINEAR_COLORSPACE=1`. On the large-source
group its output was byte-identical to the default configuration.

At q80 oximg produces 35.4 KB (Kodak group mean); imgproxy reaches a
comparable score at q90 with 63.8 KB.

Scores with oximg's speed mode (`OXIMG_RESIZE=srgb OXIMG_DCT_MARGIN=1.0`)
are ~60 on the medium group — the same level as imgproxy's and imagor's
defaults. Throughput for both settings is in [../../BENCH.md](../../BENCH.md).

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
