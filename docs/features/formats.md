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
resident, wherever the in-tree row kernel is that resize:

| layout (all decoded to 8 bits) | arm64 | amd64 (AVX2+FMA) |
|---|---|---|
| with alpha: RGBA, gray+alpha, palette or gray with `tRNS`, 8- or 16-bit | streams | streams |
| opaque: RGB, gray, palette without `tRNS`, 8- or 16-bit | streams | full frame (pic-scale resize, #154) |

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
