# Formats

Sources are sniffed by magic bytes. Extensions are never trusted.

| Format | Decode | Encode | Default output |
|---|---|---|---|
| JPEG | baseline, extended sequential (SOF1) & progressive, grayscale, CMYK/YCCK | jpegli progressive (default; sequential up to `OXIMG_JPEG_SEQUENTIAL_PIXELS`, 12k) or sequential (SOF1, `OXIMG_JPEG_PROGRESSIVE=0`); above `OXIMG_JPEG_STREAMING_PIXELS` (16 MP) sequential on standard Huffman tables, encoded row by row; `PRESET=fast\|small` → mozjpeg | itself |
| PNG | palette / gray / 16-bit → RGB(A)8 | lossless RGB(A); opt-in quantize | itself |
| WebP | lossy & lossless, alpha | lossy + alpha; canvas ≤ 16383 px | itself |
| AVIF (`--features avif`) | dav1d 8/10/12-bit, all subsamplings, alpha | SVT-AV1 10-bit 4:2:0, tune=ssim, alpha auxiliary | itself |
| GIF | GIF87a/89a, frames composited onto the logical screen | **none** | WebP (animated GIF → animated WebP) |

PNG has no shrink-on-load. A non-interlaced, upright PNG resized in
linear light as one band (`OXIMG_PAR=1`, or under two output rows per
band) streams its rows into the resize, so only a window of rows is
resident. That holds for every 8-bit layout the decoder normalizes to
(RGB, RGBA, gray, gray+alpha; 16-bit and palette sources included),
on arm64 and on an amd64 CPU with AVX2+FMA (an older one resizes
full-frame with pic-scale). Both run the same row kernel, which every
other format's single-band linear-light resize runs too (#154).

Everything else decodes to a full frame first: interlaced sources, an
eXIf orientation other than 1, sRGB mode (`OXIMG_RESIZE=srgb`),
band-parallel requests, `OXIMG_RESIZE_BACKEND=fir`, and exact-size requests (no resize; the frame
is handed to the encoder). Bytes are the same either way.

Accepted `@{fmt}` tokens: `jpg` / `jpeg` / `png` / `webp` / `avif`.
`@gif` / `@jxl` / `format=gif` / CLI `.gif` without `-f` are refused
(400 / exit 2), never answered with a different codec under that name.
See [docs/gif-evaluation.md](../gif-evaluation.md).

## Cross-format

`photo.jpg@webp`, `format=webp`, CLI `-f webp` / `.webp` extension. Any decode column combines with any encode column except GIF-out.

Alpha → JPEG flattens in linear light onto `OXIMG_FLATTEN_BG` (default white).

AVIF at default quality is often *larger* than WebP on photographs. Prefer `OXIMG_AUTO_FORMAT=webp,avif` (or `webp`) when the goal is bytes; put `avif` first only after measuring the corpus.

## Verify

```sh
oximg-ctl get /resize/100/100/photo.jpg          # image/jpeg, 100×75
oximg-ctl get /resize/100/100/photo.jpg@webp     # image/webp
oximg-ctl get /resize/100/100/still.gif          # image/webp (still)
oximg-ctl get /resize/100/100/anim.gif           # animated webp
oximg-ctl get /resize/100/100/photo.jpg@gif --expect 400
oximg-ctl probe tests/fixtures/anim.gif          # 3 frames, 1500ms
```

AVIF cells need a build with `--features avif` (`photo.avif` fixture).

## GIF animation decisions

`OXIMG_MAX_ANIM_DECODE_WORK` is optional (positive integer). The header
scan sums `max(logical screen area, frame rectangle area)` for each source
frame with saturating arithmetic. This prices both small updates on a
large canvas and rectangles that the decoder expands before clipping.
It is independent of output size and `OXIMG_ANIM_FRAME_STEP`; equal to
the cap passes. It is a work proxy, not a deadline or memory cap.
Over it, only the first frame is rendered, subject to the usual source
pixel/decoded-bytes limits. A still source is unaffected by this cap.

Under `OXIMG_TIMING`, `oximg-ctl resize` adds `gif_animation` from the
child's dedicated JSON diagnostic. The fields are `outcome`,
`source_frames`, `decode_work`, `encode_work` (potential encoded frames ×
output area), and `submitted_frames` (one for a still, otherwise frames
submitted to the final encoder before any further merging). Source/work fields are
null when policy skips the scan. The scan skips LZW payloads, so its frame
count is metadata parsed, not a guarantee all those frames decode.

Stable outcome codes (new codes may be added):

| Code | Meaning |
|---|---|
| `animated` | Animation encoder completed; libwebp may merge frames further, and truncated inputs retain the existing keep-decoded-frames policy |
| `disabled` | Animation disabled; source not scanned |
| `target_still` | Output is not WebP; source not scanned |
| `single_frame` | Fewer than two source frames, or empty canvas |
| `frame_step` | Stepping leaves fewer than two candidate frames |
| `frame_limit` | Source frame count exceeds `OXIMG_MAX_ANIM_FRAMES` |
| `decode_work_limit` | Source work exceeds `OXIMG_MAX_ANIM_DECODE_WORK` |
| `encode_work_limit` | Output work exceeds `OXIMG_MAX_ANIM_WORK` |
| `memory_limit` | Animation estimate exceeds `OXIMG_MAX_DECODED_BYTES` |
| `duration_limit` | Duration cannot fit WebP encoder timestamps |
| `single_output` | Fewer than two frames submitted after decoding (duplicates or incomplete source) |

The first matching decision is reported, not every exceeded limit.
Only successful calls produce a diagnostic or increment
`oximg_gif_output_total{outcome}` under `/metrics`. Still GIFs count too;
coalesced followers and failed requests do not. This is not a count of
lost animations: disabled/non-WebP calls intentionally skip detection.

```sh
oximg-ctl --env OXIMG_TIMING=1 --env OXIMG_MAX_ANIM_DECODE_WORK=32399 resize tests/fixtures/anim.gif 100 100 -f webp
# gif_animation: outcome=decode_work_limit, source_frames=3, decode_work=32400,
# encode_work=22500, submitted_frames=1; probe has no animation
oximg-ctl --env OXIMG_TIMING=1 --env OXIMG_MAX_ANIM_DECODE_WORK=32400 resize tests/fixtures/anim.gif 100 100 -f webp
# outcome=animated; probe.animation: 3 frames, 1500ms, loop_count=0
```
