# How to get there

Three surfaces, one pipeline. Prefer the surface the change actually
ships on; the others are cross-checks.

## HTTP (the server)

```sh
cargo build --release
./target/release/oximg-ctl get /resize/100/100/photo.jpg
./target/release/oximg-ctl get /resize/100/100/photo.jpg@webp --write /tmp/out.webp
./target/release/oximg-ctl --env OXIMG_OPTIONS_PREFIX=/image get /image/width=100/photo.jpg
./target/release/oximg-ctl matrix --source photo.jpg --box 100x100
```

Auto-spawns `oximg` on `PORT=0` with `IMAGES_DIR=tests/fixtures`
unless `--base URL` points at an already-running server. Proof is
`status`, `content_type`, `probe.{width,height}`, `sha256`. A JPEG body
also gets `probe.jpeg.{sof,progressive}`: the SOFn frame number (0
baseline, 1 extended sequential, 2 progressive), which is how a knob
that picks the scan layout is proved.

## CLI (one-shot, no HTTP)

```sh
oximg-ctl probe tests/fixtures/photo.jpg   # probe.{orientation,display_width,display_height}
oximg-ctl resize tests/fixtures/photo.jpg 100 100 --out /tmp/out.jpg
oximg-ctl resize tests/fixtures/anim.gif 100 100 -f webp --out /tmp/out.webp
oximg-ctl strip tests/fixtures/animated.webp --out /tmp/out.webp
oximg-ctl strip rotated.jpg --auto-rotate   # rotated, not_rotated, rotation{...}
```

`resize` shells out to `oximg resize` (same argv a user types). Usage
errors exit 2; processing failures exit 1. `0` on an axis is
unconstrained; `0 0` re-encodes at native size (CLI only — the server
refuses `0/0`). Its JSON carries `peak_rss_bytes` (the child's peak
RSS) and, under `--env OXIMG_LOG_DECODED_BYTES_ABOVE=1`,
`decoded_bytes.{format,total,terms}` — the estimate beside what the
process held. `--icc keep|strip` and `--[no-]auto-rotate` pass through
in the CLI's spelling; `icc_bytes` is the output's embedded profile
block (payload plus framing), null when there is none, and
`probe.{width,height}` shows whether the orientation was applied.

`strip` shells out to `oximg strip <in> <out> [--icc keep|strip]
[--auto-rotate]`:
lossless metadata removal for JPEG/PNG/WebP (GIF, AVIF: exit 1). Its
JSON proves the result: `removed` (what went, per block kind),
`orientation` (read back from the output, 2-8, else null),
`matches_library` (the output is byte for byte the library's strip of
the input, which pins the kept orientation and ICC profile), `residue` (what a second
strip under the same ICC policy would remove; must be `[]`),
`pixels_identical` (decoded pixels of input and output compared; a
PNG at its native bit depth),
`frames_identical` (animated WebP: every frame's bitstream compared,
else null), `peak_rss_bytes` (O(file size)). `ok` is false unless the
residue is empty, both sides decode, pixels and frames match, the
orientation survives, and the output matches the library's. The
input is read before the child runs, so `--out` may be the input.
`--env OXIMG_*` knobs reach ctl's own library checks as well as the
child, so a limit decides both sides alike. A CMYK JPEG
with `--icc strip` exits 1 (refused, never written).

With `--auto-rotate`, `rotated` and `not_rotated` are the library's
answer for the input under the same knobs, the reference the output
must match (`matches_library`); `orientation` is what the output
actually carries. `rotated` is the orientation the strip applies to a
JPEG's DCT blocks (2-8, else null), and `not_rotated` why a rotated
source is left tagged (an edge that is not whole 8/16-px blocks, a corrupt
stream, PNG/WebP, over `OXIMG_MAX_SRC_PIXELS`, or over
`OXIMG_MAX_DECODED_BYTES`: the coefficients, twice for every turn but
a horizontal flip, plus the compressed input, copy and output); the file is then
exactly the plain strip. A rotated output cannot match the stored
pixels, so `pixels_identical` is null and `rotation` proves it:
`round_trip_identical` (the output tagged with the inverse orientation
and rotated back decodes to the input's pixels exactly: the
coefficients are untouched) and `max_diff_displayed` (largest sample
difference against the input decoded upright; the decoder's rounding
is not rotation-symmetric, at most 3 measured; `ok` needs ≤ 8, a wrong
turn differs by 150+).

## Library (`oximg::pipeline`)

```sh
cargo run --release --example thumbnail -- tests/fixtures/photo.jpg 100 100 /tmp/out.jpg
cargo run --release --example probe -- tests/fixtures/photo.webp
```

`process` / `process_path` take `Params` and return `(bytes, ImageFormat)`.
`strip` takes `StripParams` (`icc: Option<bool>`, resolved like
`Params::icc`; `auto_rotate: bool`, default off) and returns `Stripped
{bytes, format, removed, orientation, rotated, not_rotated}` without
decoding pixels, unless `auto_rotate` turns a JPEG upright: that reads
and rewrites its DCT coefficients (`orientation` is then None and
`rotated` the orientation applied).
`probe` is header-only stored size; `probe_reader` is the same answer
from a reader, read 64 KiB at a time, doubling, until the header
parses (errors only at EOF; AVIF reads through EOF), and
`probe_step` is that loop one step at a time for callers doing their
own reads; `probe_header` / `probe_header_reader` / `probe_header_step`
add the orientation (`Header {format, width, height, orientation}`,
`display_width()`/`display_height()`), reading a WebP that flags an
`EXIF` chunk through EOF; `probe_header_seek` answers the same from a
seekable source but seeks over such a WebP's image data (the gem's
path and descriptor probes); `probe_animation` reports frames for GIF/WebP. Embedders set
`default-features = false` to drop the HTTP stack. Failures are
`pipeline::Error`; match `kind()`, wildcard arm required (`ErrorKind`
is `#[non_exhaustive]`).

A library-only change still needs a binary-level cell if the HTTP or
CLI contract can see it (status, content-type, CLI exit code).
