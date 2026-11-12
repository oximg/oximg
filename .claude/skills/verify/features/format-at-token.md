# Format / `@{fmt}` / GIF→WebP

Output format is chosen by an exact `@{fmt}` token on the positional
filename, by `format=` on the options route, or by CLI `-f`. GIF is
decode-only and becomes WebP. Catalog:
[docs/features/formats.md](../../../../docs/features/formats.md),
[routes.md](../../../../docs/features/routes.md) (Precedence).

## Sub-features

- Tokens: `jpg`/`jpeg`, `png`, `webp`, `avif` (AVIF needs
  `--features avif`). Exact token only: `photo@2x.jpg` is a filename.
- `@gif` and `@jxl` → 400. Unknown `@bogus` stays in the filename →
  404 if that literal file is absent.
- Default (no token, negotiation off): source format, except GIF →
  WebP. Animated GIF → animated WebP unless an animation budget trips
  (still 200, first frame).
- Precedence: explicit `@{fmt}` / `format=` > `Accept` negotiation
  (`OXIMG_AUTO_FORMAT`) > source format.
- Negotiation off (default): no `Vary`. On: `Vary: Accept` on every 200.
- Skip AVIF cells unless this environment already built with
  `--features avif`. Default proof: JPEG/PNG/WebP + GIF fixtures.

## How to get to it (user POV)

```
GET /resize/100/100/photo.jpg@webp
GET /resize/100/100/still.gif          # still WebP
GET /resize/100/100/anim.gif           # animated WebP
```

CLI: `oximg resize in.gif 100 100 out.webp` or `-f webp`.

## Driving it with oximg-ctl

```sh
CTL=./target/agent/oximg-ctl

$CTL get /resize/100/100/photo.jpg --expect 200
# image/jpeg, 100×75

$CTL get /resize/100/100/photo.jpg@webp --expect 200
# content_type image/webp, probe.content_type image/webp, 100×75

$CTL get /resize/100/100/still.gif --expect 200
# image/webp (still)

$CTL get /resize/100/100/anim.gif --expect 200
# image/webp; probe.animation.frames == 3 (see invariants.md Animation)

$CTL get /resize/100/100/photo.jpg@gif --expect 400
$CTL get /resize/100/100/photo.jpg@bogus --expect 404

$CTL matrix --source photo.jpg --box 100x100 --format source --format webp
$CTL probe tests/fixtures/anim.gif

$CTL --env OXIMG_TIMING=1 --env OXIMG_MAX_ANIM_DECODE_WORK=32399 \
  resize tests/fixtures/anim.gif 100 100 -f webp
# gif_animation.outcome == decode_work_limit, decode_work == 32400;
# encode_work == 22500, submitted_frames == 1; probe has no animation

$CTL --env OXIMG_TIMING=1 --env OXIMG_MAX_ANIM_DECODE_WORK=32400 \
  resize tests/fixtures/anim.gif 100 100 -f webp
# gif_animation.outcome == animated; probe.animation.frames == 3,
# duration_ms == 1500. Full contract: formats.md#gif-animation-decisions
```

Do not add `--format avif` unless this `oximg` was built with
`--features avif`. That cell transcodes `photo.jpg` (any decodeable
source); it does not need the `photo.avif` fixture. Use `photo.avif`
only when proving AVIF *decode*.

## Gotchas

- `@jpeg` on a JPEG source must match the bare URL's bytes (same-format
  path), pinned in `tests/server.rs` `explicit_format_token_transcodes`.
- A signature over `/photo.jpg` does **not** authorize `@webp`
  ([url-signing.md](url-signing.md)).
- Options-prefix filenames are literal: no `@{fmt}` on that route;
  `format=` owns the choice ([options-prefix.md](options-prefix.md)).
- Animation that degrades to a still is **200** when that first frame
  passes the ordinary source pixel and memory caps.
