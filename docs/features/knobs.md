# Knobs

Everything is environment, read once at startup. **Fail-closed**: set
but unparseable or out of range refuses to boot and names the variable.
Lenient exceptions (the process still boots):

- `OXIMG_AUTO_FORMAT` skips unknown or build-unavailable tokens with a warning
- `PRESET` maps anything other than `fast`/`small` to jpegli
- `OXIMG_TIMING` is presence-based (any set value enables), not `0`/`1`

Validated booleans are `0`/`1` only. Long form: [README Configuration](../../README.md#configuration).
Pipeline knobs are pinned to that README by `src/config.rs`
(`knobs_are_documented`).

Pass extras to a spawned server with `oximg-ctl --env KEY=VAL …`.

## Process / HTTP (live in `src/main.rs`)

| Variable | Default | One line |
|---|---|---|
| `PORT` | `8081` | `0` = OS-assigned; printed on stderr |
| `OXIMG_BIND` | `0.0.0.0` | Listen address; ctl auto-spawn sets `127.0.0.1` |
| `IMAGES_DIR` | `./images` | Local sources when no source URL |
| `OXIMG_OPTIONS_PREFIX` | unset | Mount Cloudflare-style options route |
| `OXIMG_KEY` / `OXIMG_SALT` | unset | Hex HMAC; both or neither |
| `OXIMG_WORKERS` | observed parallelism | CPU permits, 1–512. `oximg-ctl` spawn sets `1` if unset |
| `OXIMG_FETCH_CONCURRENCY` | default `min(4 × permits, 256)`; explicit 1–1024 | Concurrent origin downloads |
| `OXIMG_LOG` | `error` | `request` also logs 200s |
| `OXIMG_METRICS` | `0` | `1` serves `/metrics` |
| `OXIMG_SOURCE_BASE_URL` | unset | `https://…` or `gs://bucket[/prefix]` |
| `OXIMG_GCS_ENDPOINT` | GCS default | Emulator / PSC |
| `OXIMG_AUTO_FORMAT` | unset | `avif,webp` preference list |
| `QUALITY` | `80` | JPEG quality (process-wide) |
| `PRESET` | `jpegli` | `fast` / `small` select mozjpeg |
| `OXIMG_PAR` | `1` | Resize threads per request |

## Pipeline (`src/config.rs`)

| Variable | Default | One line |
|---|---|---|
| `OXIMG_TIMING` | unset | Per-stage stderr timings |
| `OXIMG_RESIZE` | `linear` | `srgb` disables linear-light |
| `OXIMG_RESIZE_BACKEND` | `kernel` | `fir` = portable convolution |
| `OXIMG_AUTO_ROTATE` | `1` | `0` = stored orientation |
| `OXIMG_ICC` | `1` | `0` strips profiles |
| `OXIMG_DCT_MARGIN` | unset | Shrink-on-load; speed, not quality |
| `OXIMG_JPEG_PROGRESSIVE` | `1` | `0` = baseline jpegli |
| `OXIMG_FLATTEN_BG` | `ffffff` | Alpha→JPEG background |
| `OXIMG_PNG_EFFORT` | path-dependent | `fastest`/`fast`/`balanced`/`high` |
| `OXIMG_PNG_QUANTIZE` | `0` | `1` palette-quantizes opaque PNG |
| `OXIMG_PNG_QUANTIZE_COLORS` | `256` | Palette size 2–256 |
| `OXIMG_WEBP_QUALITY` | `75` | |
| `OXIMG_WEBP_EFFORT` | `2` | libwebp `method` |
| `OXIMG_WEBP_DECODE_THREADS` | `1` | `0` disables 2-thread decode |
| `OXIMG_AVIF_QUALITY` | `55` | libavif semantics |
| `OXIMG_AVIF_ALPHA_QUALITY` | color quality | |
| `OXIMG_AVIF_SPEED` | `8` | SVT preset |
| `OXIMG_AVIF_DECODE_THREADS` | arch-dependent | 2 on x86-64, 1 elsewhere |
| `OXIMG_MAX_SOURCE_BYTES` | 64 MiB | Compressed cap → 413 |
| `OXIMG_MAX_SRC_PIXELS` | 64e6 | Header-parsed `w*h` cap → 413 |
| `OXIMG_MAX_DECODED_BYTES` | unset | Estimated decode allocation → 413 |
| `OXIMG_LOG_DECODED_BYTES_ABOVE` | unset | Name expensive decodes; still serve |
| `OXIMG_UPSTREAM_CONNECT_TIMEOUT` | `5` | Seconds |
| `OXIMG_UPSTREAM_TIMEOUT` | `30` | Whole fetch; timeout → 504 |
| `OXIMG_OVERLAP` | `auto` | Fuse JPEG decode with resize+encode |
| `OXIMG_GIF_ANIMATION` | `1` | `0` = still first frame |
| `OXIMG_MAX_ANIM_FRAMES` | `200` | Over → still 200, not 413 |
| `OXIMG_MAX_ANIM_WORK` | `8e6` | encoded frames × post-resize area |
| `OXIMG_ANIM_FRAME_STEP` | `1` | Encode every Nth frame |

Adding a pipeline knob: field on `Config` **and** `Resolved` if it has
a per-call override, README row, `KNOBS` entry, fail-closed arm in
`validate()`, a test. Adding a server-only knob: `main.rs` + README +
this table.
