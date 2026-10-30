# Invariants

These are the proofs. A refactor that changes any of them is a product
change and needs a changelog.

## Geometry

- Never enlarges. The box is a bound, not a target.
- Aspect ratio preserved. `0` on one axis leaves that axis free.
- Target box applies to the **displayed** frame (after orientation).
- WebP output is additionally scaled to fit the 16383 px side limit.
- `tiny.jpg` (40×30) into 100×100 stays 40×30.

```sh
oximg-ctl get /resize/100/100/photo.jpg   # 200×150 source → 100×75
oximg-ctl get /resize/100/100/tiny.jpg    # stays 40×30
```

## Color and samples

- Resize in linear light on 16-bit samples, Lanczos3. `OXIMG_RESIZE=srgb` disables.
- Alpha is premultiplied before the resample, unpremultiplied after.
- A still frame keeps its alpha channel only if a pixel uses it. An
  all-255 alpha is dropped after the decode. Animated output keeps
  RGBA frames.
- JPEG sources decode at full size by default, except that 4:2:0 and
  grayscale sources reduced ~3.8× or more decode luma at 1/2 through a
  linear-light 2×2 average of the full IDCT (`OXIMG_LINEAR_SHRINK`;
  0.3 SSIMULACRA2 at 4×, ~0 from 5.3×). `OXIMG_DCT_MARGIN` is a
  **speed** knob: against a lossless ground truth and a linear-light
  reference, libjpeg's 3/8 scale costs 6.4 SSIMULACRA2 points on a 5.3×
  downscale, same bytes, and full decode is best at every ratio from
  2× to 14×. (An sRGB reference favors the 1/4 and 1/2 scales instead;
  see #60.) Off unless asked.
- SIMD kernels (AVX2 / NEON) are verified against an f64 reference.
  Both architectures are production code; CI runs both.

## Orientation

Every source format auto-rotates (JPEG EXIF, PNG `eXIf`, WebP `EXIF`,
AVIF `irot`/`imir`). Output pixels are upright; the metadata is not
forwarded. `OXIMG_AUTO_ROTATE=0` serves the stored orientation.
`probe` reports **stored** size, so orientations 5–8 disagree with
`process` output axes.

## ICC

A source profile passes through byte-for-byte into any output format,
cross-format included. RGB pixels are never color-converted.
`OXIMG_ICC=0` strips.

**Exception:** CMYK/YCCK JPEG is converted to sRGB (browsers cannot
paint CMYK), through the embedded profile when present.

## Animation

Animated GIF → WebP keeps the animation unless a budget is exceeded,
in which case it degrades to the still first frame and still answers
**200** (not 413). Budgets: `OXIMG_GIF_ANIMATION`, `OXIMG_MAX_ANIM_FRAMES`,
`OXIMG_MAX_ANIM_WORK`, `OXIMG_ANIM_FRAME_STEP`. Loop counts are
translated (GIF repeats-after-first → WebP total plays).

```sh
oximg-ctl probe tests/fixtures/anim.gif
oximg-ctl get /resize/60/60/anim.gif   # probe.animation.frames == 3
```

## Identity gates (when pixels must not change)

- `bench/bytecmp.sh` — byte-identity across builds for metadata-free sources.
- Same-binary env toggle (`bench/toggle_ab.sh`) before treating a ~1%
  bench delta as real. See [bench/METHODOLOGY.md](../../bench/METHODOLOGY.md).
- Quality: [bench/quality/QUALITY.md](../../bench/quality/QUALITY.md).
  ~2 SSIMULACRA2 points is the perceptible threshold.
