# Test fixtures: provenance and regeneration

Tests depend on these files' *properties* (dimensions, color layout,
metadata), not their exact bytes — regenerated files with the same
properties are drop-in replacements unless a test says otherwise.

## Base images (0.2.0 era, hand-authored)

All 200×150 unless noted. `photo.jpg` is the photographic source; the
PNG family covers the decoder's color-type matrix (`rgb`, `rgba` with
an alpha gradient transparent-left→opaque-right, `gray`, `graya`,
`palette`, `gray16`, `rgb16`, `interlaced`); `photo.webp`/`alpha.webp`
are lossy and alpha WebP; `photo.avif`/`alpha.avif` were encoded with
`avifenc` from the corresponding sources (`alpha.avif` from
`rgba.png`, preserving the alpha gradient); `animated.webp` is two
full-canvas 64×48 frames; `tiny.jpg` is 40×30 (upscale guard). No
regeneration recipe exists — treat them as originals.

## Generated fixtures (0.4.x, reproducible)

Everything below is produced by `./regen.sh` (Docker + `alpine`
`libavif-apps`/ImageMagick — independent third-party writers, which is
the point: they pin *our* readers against *their* layouts).

| File | Recipe (see regen.sh) | Pinned by |
|---|---|---|
| `icc.avif` | `avifenc --icc <900-byte deterministic blob>` | `libavif_authored_icc_is_extracted` (byte-equal profile) |
| `orient_irot1.avif` | `avifenc --irot 1` over the corner image | `avif_irot_imir_match_libheif_rendering` |
| `orient_imir0.avif` | `avifenc --imir 0` | 〃 |
| `orient_imir1.avif` | `avifenc --imir 1` | 〃 |
| `orient_irot1_imir1.avif` | `avifenc --irot 1 --imir 1` | 〃 |
| `orient_irot3_icc.avif` | `avifenc --irot 3 --icc <blob>` | rotation+ICC composition |
| `anim.avif` | `avifenc --fps 2` over red/blue/green frames | `animated_avif_renders_first_frame` |
| `anim_meta.avif` | 〃 plus `--irot 1 --icc <blob>` | metadata on animated sources |
| `list.txt` | plain text | undecodable-input (422) test |

## CMYK/YCCK fixtures (0.6.0 era, reproducible)

64×48 with 16px corner blocks (same TL=R/TR=G/BL=B/BR=W-on-gray
layout, authored by ImageMagick, then separated to CMYK by the writer
named below). The `.ppm` references are `djpeg <fixture>` renderings —
djpeg's naive CMYK→RGB composite is byte-identical to ImageMagick's
non-color-managed rendering and is what browsers do for profile-less
CMYK, so `naive_conversion_matches_djpeg_reference` compares full
frames at tolerance ≤1. Verified marker facts the tests rely on:

| File | Recipe | Layout |
|---|---|---|
| `cmyk_ycck.jpg` | `magick corner.png -colorspace CMYK -quality 95` | Adobe APP14 transform=2 (YCCK), comps 1-4 @1×1 |
| `cmyk_t0.jpg` | `vips copy cmyk_ycck.jpg 'cmyk_t0.jpg[Q=95,strip]'` | APP14 transform=0 (plain CMYK), comps 'C','M','Y','K' |
| `cmyk_prog.jpg` | `jpegtran -progressive cmyk_ycck.jpg` | SOF2; exact coefficient twin — shares `cmyk_ycck.ppm` |
| `cmyk_noadobe.jpg` | python surgery dropping the 0xEE segment from `cmyk_t0.jpg` | no Adobe marker; classified CMYK — shares `cmyk_t0.ppm` |
| `cmyk_sub.jpg` | `magick … -quality 85 -sampling-factor '2x2,1x1,1x1,2x2'` | YCCK, Y+K @2×2, Cb/Cr @1×1 |

(ImageMagick cannot write transform=0 — it always separates to YCCK
on JPEG write; vips is the transform=0 writer. `jpegtran -copy none`
cannot strip APP14 either — libjpeg re-emits it for CMYK, hence the
python surgery.)

Two invariants the recipes must keep:

- The ICC blob is `fake_icc(900)` from `tests/common/mod.rs` —
  `(i * 131 + 7) % 251` over 900 bytes. Tests compare byte-for-byte.
- The corner image is 240×180 with 60px corner blocks TL=red,
  TR=green, BL=blue, BR=white on gray (`corner_base` in
  `tests/common/mod.rs`); orientation tests classify those corners
  after rotation.

The expected corner arrangements in `avif_irot_imir_match_libheif_
rendering` are **libheif's rendering** (captured via ImageMagick's
heic delegate). `avifdec` does *not* apply irot/imir — do not use it
to regenerate expectations.
