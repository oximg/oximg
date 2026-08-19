# GIF support: evaluation and recommendation

This document evaluates how to accept GIF — what the mainstream GIF
optimization techniques are, which of them fit this codebase, and what
each costs. It was written when `ImageFormat::sniff` deliberately
returned `None` for `GIF89a`. Every number below was measured on
**starship** (AMD Ryzen 7 8745HS, Arch Linux) on 2026-08-18 with a
purpose-built POC that links oximg's own crate graph. Nothing here is
quoted from an encoder's documentation.

**Status (2026-08-18): Tiers 0 and 1 are shipped**, so the sections
below are the reasoning behind the code, not a plan. One deliberate
divergence from imgproxy (§8): `OXIMG_GIF_ANIMATION` defaults to **on**.
The budgets are what bound the cost here, and every one of them degrades
to the still first frame rather than failing, so animation does not need
to be opt-in to stay safe — an operator who wants imgproxy's still-only
default sets the knob to `0`. See the
[Animation](../README.md#animation) section for the shipped knobs and
their defaults. Tier 2 was not built and Tier 3 remains deferred.

## Recommendation

| Tier | Scope | Verdict |
|---|---|---|
| 0 | Accept GIF, serve the **first frame** re-encoded in the target format | **Do it.** Cheap, safe, matches the existing `webp_first_frame` policy for animated WebP. |
| 1 | Animated GIF → **animated WebP**, gated by frame/pixel budgets | **Do it, behind budgets.** ~3.2x smaller than the best GIF-to-GIF result at equal quality, no new dependency. |
| 2 | GIF → **optimized GIF** | **Don't build.** Loses to gifsicle on quality-per-byte, and gifsicle itself gains ≈0% on real-world already-optimized GIFs. Pass through or fall back to Tier 0. |
| 3 | Animated **AVIF** | **Defer.** Smallest by far, but needs an in-tree ISOBMFF sequence muxer; `avif-serialize` 0.8 is still-image only. |

Independent corroboration: imgproxy 4.0.11, benchmarked on the same
corpus in §8, ships all of this and **defaults to Tier 0** — animated
GIF comes back as one frame unless `IMGPROXY_MAX_ANIMATION_FRAMES` is
raised. Its animated WebP output matches the POC's within ±3% at matched
quality, and its GIF → GIF output has a median size of 100% of the
source.

The reason Tier 1 is worth the work and Tier 2 is not: at matched quality
(SSIMULACRA2 ≈ 71 median), animated WebP lands at **25.2%** of the source
bytes while the best GIF-to-GIF variant (`gifsicle -O3 --colors 64`)
lands at **80.7%**.

## 1. What the mainstream techniques actually are

Five families, in descending order of how much they change the delivered
bytes:

**(A) Stay GIF — structural and palette optimization.** The reference
implementation is gifsicle: merge identical adjacent frames and sum
their delays, crop each frame to its changed sub-rectangle, share one
global color table, pick `Keep` disposal where possible, and re-run LZW.
`--lossy=N` additionally perturbs pixel values toward runs that LZW
compresses better; `--colors N` reduces the palette. Nothing here
changes the format — the output still plays anywhere a GIF plays.

**(B) Animated WebP.** A VP8/VP8L keyframe-plus-delta container
(`ANMF` chunks in a RIFF file, written via libwebp's `WebPAnimEncoder`).
Drop-in for `<img>`: no markup change, universal browser support.
Supports lossy and lossless, alpha, and per-frame disposal, and it does
inter-frame prediction, which GIF cannot.

**(C) Animated AVIF.** An AV1 image sequence in an ISOBMFF container.
Best compression of everything measured. Decoder support in browsers is
good but not universal, and CPU cost is the highest.

**(D) MP4/WebM video.** What Twitter/Imgur/Giphy actually serve. It
breaks `<img>` — needs `<video autoplay muted loop playsinline>` — so
it is a markup decision, not an image-server decision.

**(E) Static first frame.** Throw the animation away. The floor for
bytes and CPU, and often the right answer for a thumbnail.

## 2. Measurement method

**Corpus** — 15 real GIFs, deliberately spanning the three content
classes that turn out to decide the answer. `global palette` is the
global color table size; `sub-rect frames` counts frames smaller than
the logical screen (i.e. already structurally optimized at the source).

| file | dims | frames | source bytes | global palette | sub-rect frames | class |
|---|---|---:|---:|---:|---:|---|
| `Animhorse.gif` | 307x230 | 8 | 25 KB | 16 | 7 | flat |
| `chart_800x450_bars.gif` | 800x450 | 30 | 15 KB | 256 | 29 | flat |
| `ui_640x360_screencast.gif` | 640x360 | 36 | 15 KB | 256 | 35 | flat |
| `Muybridge_race_horse_animated.gif` | 300x200 | 15 | 555 KB | 256 | 0 | photo |
| `Newtons_cradle_animation_book_2.gif` | 480x360 | 36 | 301 KB | 256 | 35 | photo |
| `Rotating_earth_large.gif` | 400x400 | 44 | 978 KB | 256 | 43 | photo |
| `astro_401x277_comet.gif` | 401x277 | 37 | 2609 KB | 256 | 1 | photo |
| `Sunflower_as_gif_websafe.gif` | 250x297 | 1 | 27 KB | 64 | 0 | still |
| `hd_1280x720_mars.gif` | 1280x720 | 26 | 16014 KB | 256 | 0 | video |
| `vert_480x640_bee.gif` | 480x640 | 80 | 15544 KB | 256 | 0 | video |
| `vert_480x640_phone.gif` | 480x640 | 14 | 3454 KB | - | 0 | video |
| `web_356x200_metro.gif` | 356x200 | 107 | 3567 KB | - | 0 | video |
| `web_440x248_timelapse.gif` | 440x248 | 84 | 3194 KB | 256 | 0 | video |
| `web_480x270_docu.gif` | 480x270 | 265 | 18086 KB | 256 | 0 | video |
| `web_625x500_fire.gif` | 625x500 | 68 | 3905 KB | 256 | 0 | video |

**Quality metric** — SSIMULACRA2 (`ssimulacra2_rs`) over 5 frames
sampled at fixed *timestamps* from both the source and the candidate,
then averaged. Time-based sampling is not a detail: gifsicle `-O3`
legitimately merges identical adjacent frames (`web_480x270_docu`: 265
images → 119, same 26.5 s duration), so index-based sampling compares
frame *k* of one file against a different instant of the other and
scores an animation against itself shifted in time — that produced
scores of −774 before the metric was fixed. With time alignment,
`gifsicle -O3` and lossless WebP both score exactly **100.0**, which is
the correctness check for the harness.

**Two size modes** — `orig` (native size, transcode only) and `fit512`
(fit into 512x512, i.e. what oximg is actually for). All percentages are
output bytes as a fraction of the *source GIF* bytes.

**Two encoder sets** — reference CLIs (gifsicle 1.96, `ffmpeg`
libwebp/libaom, `gif2webp`, x264) to establish what is achievable, and a
Rust POC linking oximg's own dependencies to establish what oximg can
achieve in-process. gifsicle (GPL) and libimagequant are **baselines
only** — `deny.toml` allows permissive licenses only, so neither can
ever be linked.

## 3. Results: reference encoders

Native size, all 15 files:

| variant | bytes vs source (median) | (mean) | SSIMULACRA2 (median) | (min) |
|---|---:|---:|---:|---:|
| avif-crf40 | 5.5% | 17.2% | 53.5 | 15.6 |
| avif-crf30 | 11.0% | 28.3% | 65.9 | 58.2 |
| mp4-x264-crf23 | 16.1% | 27.1% | 70.3 | 19.5 |
| webp-q60 | 21.0% | 37.9% | 63.1 | 33.1 |
| webp-gif2webp-mixed | 23.8% | 42.6% | 71.1 | 61.2 |
| webp-q75 | 25.2% | 43.9% | 70.8 | 51.5 |
| webp-q85 | 38.3% | 62.1% | 79.0 | 70.3 |
| gif-O3-c64 | 80.7% | 77.2% | 71.0 | 19.4 |
| gif-O3-lossy80 | 81.4% | 78.0% | 83.0 | 31.9 |
| webp-lossless | 84.8% | 81.3% | 100.0 | 100.0 |
| gif-O3-lossy30 | 85.8% | 82.2% | 90.2 | 54.5 |
| gif-O3 | 100.0% | 92.8% | 100.0 | 100.0 |

**`gif-O3` has a median of 100.0%** — for 9 of the 15 files, gifsicle
`-O3` saves nothing at all, because real-world GIFs on the web have
already been through it. This single number is the argument against
Tier 2: the ceiling of "stay GIF, lossless" is zero on the traffic that
matters.

Fit into 512x512 (the 6 files large enough to shrink):

| variant | bytes vs source (median) | (mean) | SSIMULACRA2 (median) | (min) |
|---|---:|---:|---:|---:|
| avif-crf40 | 2.9% | 13.0% | 65.6 | 41.6 |
| avif-crf30 | 6.2% | 18.5% | 73.1 | 62.8 |
| mp4-x264-crf23 | 10.5% | 20.9% | 75.0 | 66.7 |
| webp-q60 | 14.1% | 32.4% | 67.7 | 61.0 |
| webp-q75 | 17.1% | 36.1% | 72.9 | 66.8 |
| webp-q85 | 27.3% | 48.7% | 81.3 | 73.0 |
| gif-O3-c64 | 49.5% | 47.5% | 56.7 | 23.3 |
| gif-O3-lossy80 | 58.8% | 50.4% | 75.9 | 45.6 |
| gif-O3-lossy30 | 58.9% | 52.8% | 79.3 | 53.6 |
| gif-O3 | 66.6% | 56.9% | 79.9 | 68.1 |
| webp-lossless | 170.3% | 172.3% | 100.0 | 100.0 |

Note the last row: **lossless animated WebP of a resized GIF is 1.7x
larger than the source GIF**. Resizing destroys the flat palette that
made the GIF compress, and lossless WebP then faithfully stores the
resulting continuous-tone noise. Lossless is only viable at native size.

### Content class decides the strategy

Median bytes-vs-source / median SSIMULACRA2, native size:

| variant | flat graphics | photographic | video-like |
|---|---:|---:|---:|
| gif-O3 | 81% / 100 | 100% / 100 | 99% / 100 |
| gif-O3-lossy30 | 81% / 100 | 92% / 82 | 72% / 86 |
| gif-O3-lossy80 | 81% / 100 | 85% / 71 | 63% / 77 |
| gif-O3-c64 | 81% / 100 | 82% / 72 | 68% / 43 |
| webp-q60 | 81% / 79 | 19% / 67 | 17% / 60 |
| webp-q75 | 82% / 85 | 23% / 74 | 20% / 65 |
| webp-q85 | 103% / 89 | 34% / 84 | 32% / 74 |
| webp-lossless | 85% / 100 | 89% / 100 | 84% / 100 |
| webp-gif2webp-mixed | 111% / 100 | 24% / 74 | 20% / 68 |
| avif-crf30 | 81% / 86 | 13% / 68 | 6% / 62 |
| avif-crf40 | 58% / 83 | 6% / 54 | 3% / 51 |
| mp4-x264-crf23 | 66% / 89 | 13% / 75 | 14% / 68 |

Read the `flat graphics` column: every transcode is roughly a wash, and
`webp-q85` is **worse than the source (103%)** while also being lossy.
For screencasts, charts, and line art, GIF's palette-plus-LZW is already
a good format and there is nothing to win. Photographic and video-like
GIFs are where 20% lives. This is why the decision must be made per
image from measured output, not per format by policy.

Per file, native size (bytes vs source / SSIMULACRA2):

| file | gif-O3 | gif-O3-lossy30 | webp-q75 | webp-gif2webp-mixed | avif-crf30 |
|---|---:|---:|---:|---:|---:|
| `Animhorse.gif` | 100% / 100 | 100% / 100 | 156% / 80 | 111% / 100 | 123% / 81 |
| `chart_800x450_bars.gif` | 81% / 100 | 81% / 100 | 82% / 88 | 21% / 100 | 34% / 92 |
| `ui_640x360_screencast.gif` | 77% / 100 | 76% / 98 | 81% / 85 | 144% / 97 | 81% / 86 |
| `Muybridge_race_horse_animated.gif` | 101% / 100 | 98% / 90 | 14% / 77 | 14% / 77 | 10% / 71 |
| `Newtons_cradle_animation_book_2.gif` | 100% / 100 | 101% / 98 | 28% / 81 | 50% / 81 | 20% / 83 |
| `Rotating_earth_large.gif` | 100% / 100 | 86% / 73 | 21% / 68 | 23% / 68 | 9% / 65 |
| `astro_401x277_comet.gif` | 100% / 100 | 43% / 61 | 25% / 71 | 25% / 71 | 16% / 60 |
| `Sunflower_as_gif_websafe.gif` | 100% / 100 | 100% / 100 | 92% / 65 | 89% / 100 | 80% / 66 |
| `hd_1280x720_mars.gif` | 100% / 100 | 98% / 86 | 19% / 68 | 19% / 68 | 3% / 66 |
| `vert_480x640_bee.gif` | 98% / 100 | 95% / 86 | 24% / 71 | 24% / 71 | 4% / 63 |
| `vert_480x640_phone.gif` | 100% / 100 | 100% / 96 | 28% / 72 | 28% / 72 | 13% / 58 |
| `web_356x200_metro.gif` | 100% / 100 | 72% / 56 | 19% / 65 | 19% / 68 | 11% / 60 |
| `web_440x248_timelapse.gif` | 91% / 100 | 69% / 75 | 15% / 52 | 15% / 62 | 3% / 62 |
| `web_480x270_docu.gif` | 44% / 100 | 44% / 91 | 20% / 65 | 20% / 65 | 11% / 66 |
| `web_625x500_fire.gif` | 99% / 100 | 69% / 55 | 36% / 61 | 36% / 61 | 6% / 59 |

## 4. Results: the POC, in oximg's own process

The POC decodes with the `gif` crate, composites frames on an RGBA
canvas honoring transparency and all three disposal methods, resizes
through oximg's existing linear-light Lanczos3 path, and encodes three
ways: first frame only, animated WebP (`WebPAnimEncoder`, q75), and an
in-tree GIF encoder (global palette from a sampled quantization, no
dither, per-frame changed-rectangle diffs). Timings are the POC's own
instrumentation, one process per file, single-threaded. Peak RSS is
`VmHWM` from `/proc/self/status`.

Fit into 512x512:

| file | frames | still (first frame) | animated WebP | ss2 | in-tree GIF | ss2 | WebP ms | GIF ms | WebP peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `Animhorse.gif` | 8 | 18% | 114% | 100.0 | 134% | 100.0 | 30 | 10 | 7 MB |
| `chart_800x450_bars.gif` | 30 | 4% | 68% | 90.4 | 93% | 92.5 | 80 | 64 | 14 MB |
| `ui_640x360_screencast.gif` | 36 | 10% | 70% | 85.7 | 96% | 94.9 | 81 | 55 | 12 MB |
| `Muybridge_race_horse_animated.gif` | 15 | 1% | 14% | 77.4 | 49% | 74.2 | 232 | 30 | 9 MB |
| `Newtons_cradle_animation_book_2.gif` | 36 | 4% | 28% | 80.8 | 53% | 54.2 | 248 | 51 | 14 MB |
| `Rotating_earth_large.gif` | 44 | 1% | 21% | 68.4 | 95% | 82.7 | 291 | 105 | 11 MB |
| `astro_401x277_comet.gif` | 37 | 1% | 25% | 71.1 | 42% | 62.9 | 334 | 140 | 11 MB |
| `Sunflower_as_gif_websafe.gif` | 1 | 92% | 89% | 100.0 | 102% | 98.6 | 24 | 4 | 8 MB |
| `hd_1280x720_mars.gif` | 26 | 0% | 4% | 72.9 | 12% | 72.8 | 517 | 522 | 43 MB |
| `vert_480x640_bee.gif` | 80 | 0% | 16% | 72.8 | 68% | 69.8 | 1483 | 774 | 38 MB |
| `vert_480x640_phone.gif` | 14 | 1% | 18% | 72.9 | 73% | 75.4 | 263 | 152 | 19 MB |
| `web_356x200_metro.gif` | 107 | 0% | 19% | 64.6 | 85% | 65.7 | 588 | 229 | 12 MB |
| `web_440x248_timelapse.gif` | 84 | 0% | 15% | 51.5 | 76% | 64.5 | 2270 | 284 | 14 MB |
| `web_480x270_docu.gif` | 265 | 0% | 20% | 65.2 | 40% | 52.2 | 3220 | 677 | 36 MB |
| `web_625x500_fire.gif` | 68 | 0% | 14% | 66.7 | 103% | 67.9 | 944 | 520 | 22 MB |

Native size:

| file | frames | still (first frame) | animated WebP | ss2 | in-tree GIF | ss2 | WebP ms | GIF ms | WebP peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `Animhorse.gif` | 8 | 18% | 114% | 100.0 | 134% | 100.0 | 28 | 10 | 7 MB |
| `chart_800x450_bars.gif` | 30 | 7% | 19% | 100.0 | 98% | 100.0 | 50 | 38 | 18 MB |
| `ui_640x360_screencast.gif` | 36 | 15% | 82% | 92.5 | 83% | 96.4 | 37 | 20 | 14 MB |
| `Muybridge_race_horse_animated.gif` | 15 | 1% | 14% | 77.4 | 49% | 74.2 | 232 | 31 | 9 MB |
| `Newtons_cradle_animation_book_2.gif` | 36 | 4% | 28% | 80.8 | 53% | 54.2 | 249 | 50 | 14 MB |
| `Rotating_earth_large.gif` | 44 | 1% | 21% | 68.4 | 95% | 82.7 | 296 | 104 | 11 MB |
| `astro_401x277_comet.gif` | 37 | 1% | 25% | 71.1 | 42% | 62.9 | 335 | 140 | 11 MB |
| `Sunflower_as_gif_websafe.gif` | 1 | 92% | 89% | 100.0 | 102% | 98.6 | 24 | 4 | 8 MB |
| `hd_1280x720_mars.gif` | 26 | 1% | 19% | 68.1 | 78% | 74.4 | 2040 | 884 | 60 MB |
| `vert_480x640_bee.gif` | 80 | 0% | 24% | 70.8 | 80% | 69.0 | 2130 | 817 | 41 MB |
| `vert_480x640_phone.gif` | 14 | 2% | 28% | 71.6 | 97% | 78.2 | 379 | 157 | 18 MB |
| `web_356x200_metro.gif` | 107 | 0% | 19% | 64.6 | 85% | 65.7 | 584 | 228 | 12 MB |
| `web_440x248_timelapse.gif` | 84 | 0% | 15% | 51.5 | 76% | 64.5 | 2271 | 284 | 14 MB |
| `web_480x270_docu.gif` | 265 | 0% | 20% | 65.2 | 40% | 52.2 | 3213 | 680 | 37 MB |
| `web_625x500_fire.gif` | 68 | 0% | 36% | 61.2 | 116% | 95.7 | 1576 | 541 | 25 MB |

Three findings:

1. **The POC's animated WebP matches the reference tools per file.**
   `Muybridge` 13.6% (ffmpeg 14%), `Rotating_earth` 20.7% (21%),
   `web_480x270_docu` 20.2% (20%). Whatever `gif2webp` does, oximg can
   do in-process.
2. **Memory is O(canvas), not O(frames).** Peak RSS is 7–60 MB across
   the corpus — the 265-frame source peaks at 36 MB, *less* than the
   26-frame 1280x720 one at 60 MB. `WebPAnimEncoder` is incremental and
   the compositor holds at most three canvases (current, previous,
   resized). Frame count does not enter the memory budget.
3. **The in-tree GIF encoder is not competitive.** Engineering it
   properly helped a lot — a global palette with no dithering and
   changed-rectangle diffs took the screencast from 137,813 bytes
   (per-frame dithered quantization) to 15,024, a 9x improvement — but
   it still lands at 40–103% of source with worse quality-per-byte than
   gifsicle. Two files come out *larger* than their source. Confirmed:
   don't build a GIF encoder.

## 5. The real risk is CPU, not bytes or memory

A 1280x720 JPEG resized to fit 512x512 and re-encoded as WebP costs
**5.0 ms** in-process on this box (oximg's own `OXIMG_TIMING=1`, mean of
15 iterations). Against that yardstick, animated WebP at native size:

| file | frames | canvas px | decode+composite ms | encode ms | total ms | = N still requests |
|---|---:|---:|---:|---:|---:|---:|
| `web_480x270_docu.gif` | 265 | 130k | 195 | 3018 | 3213 | 643x |
| `web_356x200_metro.gif` | 107 | 71k | 43 | 542 | 584 | 117x |
| `web_440x248_timelapse.gif` | 84 | 109k | 60 | 2211 | 2271 | 454x |
| `vert_480x640_bee.gif` | 80 | 307k | 165 | 1964 | 2130 | 426x |
| `web_625x500_fire.gif` | 68 | 312k | 83 | 1493 | 1576 | 315x |
| `Rotating_earth_large.gif` | 44 | 160k | 19 | 277 | 296 | 59x |
| `astro_401x277_comet.gif` | 37 | 111k | 27 | 308 | 335 | 67x |
| `Newtons_cradle_animation_book_2.gif` | 36 | 173k | 8 | 241 | 249 | 50x |
| `ui_640x360_screencast.gif` | 36 | 230k | 2 | 35 | 37 | 7x |
| `chart_800x450_bars.gif` | 30 | 360k | 4 | 46 | 50 | 10x |
| `hd_1280x720_mars.gif` | 26 | 922k | 183 | 1858 | 2040 | 408x |
| `Muybridge_race_horse_animated.gif` | 15 | 60k | 6 | 226 | 232 | 46x |
| `vert_480x640_phone.gif` | 14 | 307k | 33 | 346 | 379 | 76x |
| `Animhorse.gif` | 8 | 71k | 2 | 26 | 28 | 6x |
| `Sunflower_as_gif_websafe.gif` | 1 | 74k | 1 | 23 | 24 | 5x |

One request can cost as much as **643 still requests**. That is the
whole design constraint: a handful of concurrent 265-frame GIFs will
saturate the CPU semaphore and stall every other request behind it.
Encoding, not decoding, dominates — 3018 of 3213 ms for the worst case —
so the levers are frame count and frame area, both of which we control.

**Resizing first is the cheapest lever**, because it cuts the encoder's
pixel count: `hd_1280x720_mars` costs 2040 ms at native size and 517 ms
into a 512 box, a 3.9x reduction, and the output drops from 19% to 4% of
source at *better* quality (68.1 → 72.9).

**Frame decimation is close to linear**, measured at box 512 by dropping
every 2nd/3rd frame:

| file | step | frames encoded | bytes | encode ms |
|---|---:|---:|---:|---:|
| `web_480x270_docu.gif` | 1 | 265 | 3,746,362 | 3021 |
| | 2 | 133 | 2,000,600 | 1651 |
| | 3 | 89 | 1,401,134 | 1130 |
| `web_440x248_timelapse.gif` | 1 | 84 | 500,264 | 2218 |
| | 2 | 42 | 249,390 | 1110 |
| | 3 | 28 | 164,340 | 722 |
| `vert_480x640_bee.gif` | 1 | 80 | 2,566,004 | 1243 |
| | 2 | 40 | 1,282,922 | 627 |
| | 3 | 27 | 865,212 | 421 |
| `hd_1280x720_mars.gif` | 1 | 26 | 657,814 | 296 |
| | 2 | 13 | 332,278 | 150 |
| | 3 | 9 | 230,908 | 104 |
| `web_625x500_fire.gif` | 1 | 68 | 554,876 | 808 |
| | 2 | 34 | 274,146 | 413 |
| | 3 | 23 | 189,770 | 278 |

Caveat on how to report this: dropping frames while preserving total
duration collapses per-instant SSIMULACRA2 (`vert_480x640_bee` 72.8 →
22.8 at step 2) because the metric samples a timestamp and finds a stale
frame. That is **judder**, not an image artifact — the frames that are
present are as good as before. Decimation is a legitimate lever but it
degrades smoothness, not fidelity, and should never be applied silently
to a low-frame-rate source.

## 6. Fit for oximg

### Dependencies

**Animated WebP needs no new dependency.** `libwebp-sys` already
compiles the vendored mux and demux sources, and oximg already calls
`WebPMux*` to attach ICC profiles. Verified with `nm` on
`target/release/oximg`: `WebPMuxCreateInternal` is present and there are
**0** `WebPAnim*` symbols — while the POC binary, built from the same
crate graph with no `Cargo.toml` change, has **5**. The C code is
already in the build; it links in the moment it is referenced. What is
needed is the `unsafe extern` declarations for
`WebPAnimEncoderOptionsInitInternal` / `WebPAnimEncoderNewInternal` /
`WebPAnimEncoderAdd` / `WebPAnimEncoderAssemble` /
`WebPAnimEncoderDelete`, pinned to `WEBP_MUX_ABI_VERSION`, in the same
style as the existing mux bindings.

**The only genuinely new crate is a GIF decoder.** `gif` 0.14.2:

- License `MIT OR Apache-2.0` — passes `deny.toml` unchanged.
- `#![forbid(unsafe_code)]` — the whole decoder, including LZW, is safe
  Rust. This matters for a format whose historical CVEs are all in C
  decoders.
- One dependency, `weezl` 0.1.10+ (`MIT OR Apache-2.0`, also
  unsafe-free), plus optional `color_quant`, which we don't need.
- `rust-version = "1.62"`, well under our MSRV.
- Ships a `MemoryLimit` (default `Bytes(50_000_000)`) that is settable
  before decode — a decoder-level guard that composes with our own
  budgets.

No GIF *encoder* dependency is needed, per §4 finding 3.

### Memory accounting

`DecodeCost` / `check_decoded_bytes` / `check_src_pixels` are all
O(one frame) today, and §4 finding 2 says that is still the right shape:
the compositor and `WebPAnimEncoder` are both O(canvas). What animation
adds is a **CPU** axis, `frames × pixels`, which no existing budget
covers. That is the new term to add — a work budget, checked after
`probe()` yields the frame count and before any frame is composited, so
an oversized animation is rejected or degraded to Tier 0 *before* it
consumes anything.

### Security surface

GIF's risks are well understood and all of them are decode-side:
frame-count bombs (a small file expanding to thousands of frames), a
huge logical screen with tiny frames, sub-rectangles that claim to
extend past the canvas, and delay-0 frames. A safe-Rust decoder plus an
explicit `frames × pixels` cap plus clamping every frame rectangle to
the canvas covers all four. The compositor must treat out-of-bounds
frame rectangles as clamp-or-reject, never as trusted geometry.

### Why animated AVIF is deferred, concretely

oximg's AVIF path is SVT-AV1 for encoding plus `avif-serialize` 0.8 for
the container. SVT-AV1 can encode an image sequence, but
`avif-serialize` 0.8 is a still-image ISOBMFF writer with no track or
sequence API. Animated AVIF therefore means writing an in-tree
`moov`/`trak`/`stts`/`stsc` muxer — a real project, and the wrong one to
start with when animated WebP is free and universally supported.

## 7. Proposed integration

Tier 0 and Tier 1 touch the same places; Tier 1 adds the encoder.

**Format plumbing** (`src/pipeline/mod.rs`):

- `ImageFormat::Gif` variant; `content_type()` → `"image/gif"`.
- `sniff()` — match `GIF87a` / `GIF89a` in the first 6 bytes. The
  existing assertion at `src/pipeline/tests.rs:520` (and the negative
  test at `tests/formats.rs:91`) must be updated, not worked around.
- `format_max_dimension` — GIF's logical screen fields are `u16`, so
  65535. Only relevant if we ever emit GIF; for input it bounds `probe`.
- `from_token()` — **do not** add `"gif"` initially. Accepting `@gif`
  would promise a GIF *encoder*, which §4 says we should not ship. Leave
  GIF input-only, exactly as the format table already distinguishes
  decodable from encodable formats.
- `process_reader()` — a `Gif` arm that decodes, composites, resizes,
  and hands off to the still or animated encoder depending on the
  budgets and the negotiated target.

**Negotiation** (`src/main.rs:792`, `negotiate`, and the precedence at
`:920`): the existing order (`@fmt` / `format=` > `Accept` > source
format) already does the right thing. An animated GIF with
`Accept: image/webp` becomes animated WebP; without it, it falls back to
first-frame or pass-through. No new precedence rule is required.

**`probe`** (`src/cli.rs:144`): report logical screen size, frame count,
and total duration, so the animation budgets are inspectable from the
CLI before anything is served.

**New knobs**, consistent with the existing 36 `OXIMG_*` settings in
`src/config.rs`:

- `OXIMG_GIF_ANIMATION` — off / on. Off means Tier 0 for every
  animated GIF.
- `OXIMG_MAX_ANIM_FRAMES` — hard frame cap.
- `OXIMG_MAX_ANIM_WORK` — the `frames × pixels` budget; over it,
  degrade to Tier 0 rather than fail, matching how `webp_first_frame`
  already handles animated WebP input.
- `OXIMG_ANIM_FRAME_STEP` or an FPS ceiling — optional decimation, off
  by default given the judder caveat in §5.

**Merge duplicate adjacent frames** before encoding, summing their
delays — no knob, always on. It is the one thing imgproxy does better
than the POC (§8.2 finding 4): identical output, fewer frames encoded,
1.6x cheaper on the worst file in the corpus. Do this before the
`frames × pixels` budget check, so the budget is spent on frames that
actually differ.

**Defaults, from the measurements.** A `frames × pixels` budget in the
neighborhood of 8–10 Mpx of total frame area keeps the worst request
near 100 ms after resize (`hd_1280x720_mars` at box 512 is 26 × ~0.26 Mpx
≈ 6.8 Mpx → 517 ms; `web_480x270_docu` at box 512 is 265 × ~0.13 Mpx
≈ 34 Mpx → 3220 ms). The exact figure should be set against the target
deployment's CPU budget, but the shape is clear: budget the *product*,
apply the resize before the encode, and degrade rather than fail.

## 8. Cross-check: imgproxy 4.0.11 on the same corpus

imgproxy already ships animated-GIF support (libvips), so it answers two
questions this evaluation would otherwise have to guess at: is 20% of
source the real ceiling for animated WebP, and what does animation cost a
mature implementation? Measured 2026-08-18 on the same box:
`ghcr.io/imgproxy/imgproxy:latest` (version 4.0.11, the same image as
[BENCH.md](../BENCH.md)), one **fresh container per file**, 1 warm-up
plus 3 measured requests, CPU from the container's cgroup `cpu.stat`
(`usage_usec`), peak memory from `memory.peak`, sources mounted from the
same directory via `local://`.

Asymmetry to keep in mind: imgproxy is measured over HTTP in a container,
the POC in-process. §8.1 sizes that overhead — for animated requests
costing hundreds of milliseconds it is noise.

### 8.1 Still control: the same request on both servers

1280x720 JPEG → fit 512x512 → WebP, `ab -k -n 500 -c 1`. oximg defaults
to `OXIMG_WEBP_QUALITY=75` and imgproxy to quality 80, so both quality
points were measured on both servers. Quality is SSIMULACRA2 against the
Lanczos-downscaled source.

| server | WebP q | bytes | SSIMULACRA2 | req/s | mean ms | CPU ms/req | peak MB | idle MB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| imgproxy 4.0.11 | 75 | 8,866 | 53.5 | 63.1 | 15.9 | 16.2 | 39.4 | 20.6 |
| imgproxy 4.0.11 | 80 | 11,962 | 62.3 | 59.0 | 16.9 | 17.3 | 38.5 | 21.2 |
| oximg 0.11.0 | 75 | 9,622 | 56.0 | 135.9 | 7.4 | 9.6 | 10.4 | 10.1 |
| oximg 0.11.0 | 80 | 12,324 | 62.1 | 132.1 | 7.6 | 9.8 | 10.4 | 10.1 |

At q80 the two land on the same point in bytes (12,324 vs 11,962, +3%)
and the same quality (62.1 vs 62.3), and oximg is **2.2x faster wall,
1.8x cheaper in CPU, and 3.7x smaller in peak memory**. That is the
baseline gap; it is not a GIF finding, and it is consistent with the
existing still benchmarks in BENCH.md (113MB vs 235MB image, 10MB vs
29MB idle RSS).

A caveat on `ab` against a single URL: at `-c 8` oximg reports 1082 rps
and only 1.26 ms CPU/req because request coalescing collapses the 8
identical in-flight requests into one computation. imgproxy has no
coalescing (385 rps, 20.5 ms CPU/req). Only the `-c 1` rows above are a
like-for-like CPU comparison.

### 8.2 imgproxy's own scenarios (medians over the 15 files, fit 512x512)

| scenario | bytes vs source | ss2 | CPU ms | wall ms | peak MB | peak − idle MB |
|---|---:|---:|---:|---:|---:|---:|
| default (animation off) | 1% | 76.4 | 14 | 13 | 34 | 15 |
| animated WebP, q80 | 22% | 75.7 | 376 | 361 | 56 | 32 |
| animated WebP, q75 | 20% | 71.7 | 359 | 346 | 50 | 31 |
| animated GIF → GIF | 100% | 97.4 | 379 | 366 | 53 | 31 |
| animated WebP, native size | 29% | 73.9 | 497 | 474 | 47 | 24 |

The POC's equivalents, same files, same box:

| mode | bytes vs source | ss2 | total ms | peak RSS MB |
|---|---:|---:|---:|---:|
| first frame | 1% | 73.2 | 9 | 9 |
| animated WebP q75 | 20% | 72.9 | 291 | 14 |
| in-tree GIF → GIF | 76% | 72.8 | 140 | 16 |

Four things fall out of this, and three of them are confirmations:

1. **imgproxy's default is Tier 0.** With no animation env var set, GIF
   input comes back as a **single frame** — 1% of source bytes. Enabling
   animation takes `IMGPROXY_MAX_ANIMATION_FRAMES` (verified: setting
   only `IMGPROXY_SECURITY_MAX_ANIMATION_FRAMES=1000` still yields 1
   frame, so that name does nothing in 4.0.11). The most-deployed
   image proxy defaults to exactly the policy this document recommends
   as Tier 0, and makes animation opt-in for exactly the CPU reason in
   §5.
2. **20% is the real ceiling, not a POC artifact.** At matched quality
   (both q75) imgproxy and the POC agree within ±3% on 13 of 15 files —
   `astro_401x277_comet` 658 KB vs 658 KB, `vert_480x640_bee` 2,493 KB
   vs 2,506 KB, `hd_1280x720_mars` 623 KB vs 642 KB — with SSIMULACRA2
   within ±1.5. Both are libwebp; there is no compression advantage to
   be had on either side, only cost.
3. **GIF → GIF is confirmed pointless, from a second implementation.**
   imgproxy's resized GIF output has a **median of 100% of the source
   bytes** — and *worse* than the source on the flat-graphics files
   (`chart_800x450_bars` 227%, `Muybridge` 107%). It pays the full
   animation CPU cost (379 ms median) to deliver no savings. Our own
   in-tree attempt (76% median) is actually the better GIF encoder here,
   and §4 still says don't ship it.
4. **The one place imgproxy beats the POC is duplicate-frame merging.**
   For `web_480x270_docu` it emits **119 frames for a 265-frame source**
   (same 26.5 s duration, delays summed) — the same merge gifsicle does
   — and that alone makes it 1.6x cheaper than the POC on that file
   (1,998 ms vs 3,220 ms CPU) at equal bytes and quality. `chart` (30 →
   28) and `fire` (68 → 67) merge too. **This is a feature to copy**: it
   is pure profit — identical output, fewer frames encoded — and it
   attacks precisely the worst case in §5, where the frame count is what
   makes a request cost 643 stills.

### 8.3 Per-file cost and memory, animated WebP into 512x512

imgproxy at its default q80 (its `ss2` is correspondingly higher than
the POC's q75; the q75 columns of §8.2 are the matched comparison):

| file | src frames | ip frames | ip bytes | POC bytes | ip ss2 | POC ss2 | ip CPU ms | POC ms | ip peak MB | POC RSS MB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `web_480x270_docu.gif` | 265 | 119 | 4047 KB (22%) | 3659 KB (20%) | 66.8 | 65.2 | 2038 | 3220 | 131 | 36 |
| `web_356x200_metro.gif` | 107 | 107 | 785 KB (22%) | 670 KB (19%) | 66.5 | 64.6 | 794 | 588 | 51 | 12 |
| `web_440x248_timelapse.gif` | 84 | 84 | 598 KB (19%) | 489 KB (15%) | 64.8 | 51.5 | 879 | 2270 | 58 | 14 |
| `vert_480x640_bee.gif` | 80 | 80 | 2914 KB (19%) | 2506 KB (16%) | 75.7 | 72.8 | 2148 | 1483 | 196 | 38 |
| `web_625x500_fire.gif` | 68 | 67 | 658 KB (17%) | 542 KB (14%) | 66.7 | 66.7 | 1260 | 944 | 84 | 22 |
| `Rotating_earth_large.gif` | 44 | 44 | 228 KB (23%) | 202 KB (21%) | 70.6 | 68.4 | 376 | 291 | 41 | 11 |
| `astro_401x277_comet.gif` | 37 | 37 | 758 KB (29%) | 658 KB (25%) | 76.0 | 71.1 | 504 | 334 | 44 | 11 |
| `Newtons_cradle_animation_book_2.gif` | 36 | 36 | 117 KB (39%) | 85 KB (28%) | 82.9 | 80.8 | 222 | 248 | 39 | 14 |
| `ui_640x360_screencast.gif` | 36 | 36 | 12 KB (79%) | 11 KB (70%) | 86.9 | 85.7 | 210 | 81 | 72 | 12 |
| `chart_800x450_bars.gif` | 30 | 28 | 14 KB (94%) | 10 KB (68%) | 82.9 | 90.4 | 227 | 80 | 56 | 14 |
| `hd_1280x720_mars.gif` | 26 | 26 | 737 KB (5%) | 642 KB (4%) | 76.2 | 72.9 | 772 | 517 | 114 | 43 |
| `Muybridge_race_horse_animated.gif` | 15 | 15 | 91 KB (16%) | 75 KB (14%) | 80.5 | 77.4 | 91 | 232 | 31 | 9 |
| `vert_480x640_phone.gif` | 14 | 14 | 706 KB (20%) | 622 KB (18%) | 74.9 | 72.9 | 353 | 263 | 60 | 19 |
| `Animhorse.gif` | 8 | 8 | 41 KB (165%) | 28 KB (114%) | 80.7 | 100.0 | 42 | 30 | 30 | 7 |
| `Sunflower_as_gif_websafe.gif` | 1 | 1 | 28 KB (104%) | 24 KB (89%) | 69.1 | 100.0 | 11 | 24 | 23 | 8 |

**Memory is where the two diverge most.** imgproxy's peak for
`vert_480x640_bee` is 196 MB against the POC's 38 MB peak RSS — and the
POC figure is the whole process, while imgproxy's 196 MB is on top of a
20 MB idle baseline. Per-request delta, median: 31 MB vs ~5 MB over
idle. The `WebPAnimEncoder`-plus-one-canvas shape of §4 is materially
leaner than libvips' animated pipeline, which holds frames as one tall
strip image.

CPU is a split decision, and the split is informative: imgproxy is
cheaper exactly where it merges frames (`docu` 1,998 vs 3,220 ms at q75)
and the POC is cheaper everywhere else (median 291 vs 359 ms; `chart` 80
vs 216 ms, `screencast` 81 vs 209 ms, `mars` 517 vs 750 ms). Two POC
outliers are worth flagging as work items rather than results:
`web_440x248_timelapse` (2,270 vs 845 ms) and `Muybridge` (232 vs 91 ms)
are cases where libwebp spends disproportionate effort in the POC's
configuration — the encoder options (`minimize_size`, `allow_mixed`,
method) were never tuned, and imgproxy's numbers show the headroom.

## 9. What was deliberately not done

- No GIF encoder. §4 finding 3.
- No gifsicle or libimagequant linkage — GPL and license-incompatible;
  they exist here only as measurement baselines.
- No animated AVIF. §6.
- No MP4/WebM output: it breaks `<img>`, so it is a caller's markup
  decision, not something an image server should silently substitute.

## 10. Reproducing

The POC harness lives outside this repo (`/tmp/gifpoc` on starship as of
2026-08-18) and is not part of the build. It consists of: a Rust binary
linking oximg's crate graph with modes
`first|webp|webp-lossless|gif|gif2|gif2-c64`, `characterize.py` (GIF
structure → `corpus.json`), `run.py` (reference encoders →
`results.json`), `rescore.py` (time-aligned SSIMULACRA2), `score.py`
(POC output on the same axis), `tables.py` (§2–§5), `imgproxy_bench.sh`
(the per-file container sweep), `still_control.sh` / `still_matched.sh`
(§8.1), and `compare.py` (§8.2–§8.3).
Final state: 15 corpus files, 246 reference rows with 0 errors and 0
missing scores, 120 in-process timing rows, 15 decimation rows, 75
imgproxy rows across 5 scenarios.
