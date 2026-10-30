# Knobs

Everything is environment, read once at startup. **Fail-closed**: set
but unparseable or out of range refuses to boot and names the variable.
Lenient exceptions (the process still boots):

- `OXIMG_AUTO_FORMAT` skips unknown or build-unavailable tokens with a warning
- `OXIMG_LOG` warns on an unknown level and logs failures only, as `error`
- `OXIMG_PNG_EFFORT` warns on an unknown level and encodes as if unset
- `PRESET` maps anything other than `fast`/`small` to jpegli
- `OXIMG_TIMING` is presence-based (any set value enables), not `0`/`1`
- `OXIMG_GCS_ENDPOINT` is read when a `gs://` request is built, not at
  boot; a bad URL fails that request, not startup
- `GCE_METADATA_HOST` is read when fetching or refreshing the metadata
  token (including the startup credential probe), not snapshotted into
  `Config`
- `GLIBC_TUNABLES` is glibc's, not ours: the server only checks whether
  it names a `glibc.malloc.*` tunable, and if so leaves the allocator
  alone

`OXIMG_S3_ENDPOINT` and the `AWS_*` names are read on first use, not
at startup, but they stay fail-closed: the `s3://` boot probe reads
them, and a bad value refuses to boot.

Validated booleans are `0`/`1` only. Long form: [README Configuration](../../README.md#configuration).
Pipeline knobs are pinned to that README **and this file** by
`src/config.rs` (`knobs_are_documented`).

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
| `OXIMG_LOG` | `error` | `request` (or `info`/`debug`/`trace`) also logs 200s; unknown warns, not fatal |
| `OXIMG_METRICS` | `0` | `1` serves `/metrics` |
| `OXIMG_SOURCE_BASE_URL` | unset | `https://…`, `gs://bucket[/prefix]` or `s3://bucket[/prefix]` |
| `OXIMG_GCS_ENDPOINT` | GCS default | Emulator / PSC; read per `gs://` request, not fail-closed at boot |
| `GCE_METADATA_HOST` | Google metadata | GCS auth emulator / PSC; read when fetching or refreshing the metadata token (including the startup credential probe) |
| `OXIMG_S3_ENDPOINT` | AWS for `AWS_REGION` (China uses `.amazonaws.com.cn`) | `scheme://host[:port]` of an S3-compatible store (R2, MinIO) |
| `OXIMG_S3_PATH_STYLE` | `1` with a custom endpoint or for a bucket name with a `.`, else `0` | `0`/`1`: bucket in the path or in the host name |
| `AWS_REGION` | unset | Required for `s3://`; signed into every request. R2 accepts `auto` |
| `AWS_ACCESS_KEY_ID` | unset | `s3://` static key |
| `AWS_SECRET_ACCESS_KEY` | unset | `s3://` static key |
| `AWS_SESSION_TOKEN` | unset | `s3://` temporary keys only |
| `AWS_WEB_IDENTITY_TOKEN_FILE` | unset | `s3://` keys from EKS IRSA: the OIDC token, read again on every fetch. Needs `AWS_ROLE_ARN` |
| `AWS_ROLE_ARN` | unset | Role that STS `AssumeRoleWithWebIdentity` assumes |
| `AWS_ROLE_SESSION_NAME` | `oximg-<unix time>` | Session name in CloudTrail |
| `AWS_ENDPOINT_URL_STS` | `https://sts.<AWS_REGION>.amazonaws.com` (`.com.cn` in China) | STS endpoint for web identity |
| `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` | unset | `s3://` keys from the ECS task role; a path on `169.254.170.2`. Used when neither static keys nor web identity are set |
| `AWS_CONTAINER_CREDENTIALS_FULL_URI` | unset | `s3://` keys from EKS Pod Identity or another agent. `http://` only to loopback or the ECS or EKS agent address |
| `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE` | unset | Token for the container endpoint, read again on every fetch. Wins over `AWS_CONTAINER_AUTHORIZATION_TOKEN` |
| `AWS_CONTAINER_AUTHORIZATION_TOKEN` | unset | Token for the container endpoint, as a value |
| `AWS_EC2_METADATA_DISABLED` | `false` | `true`: do not try EC2 instance metadata (IMDSv2) for `s3://` keys. IMDS is the last source and is tried when nothing else is set |
| `AWS_EC2_METADATA_SERVICE_ENDPOINT` | `http://169.254.169.254` | IMDS address as `scheme://host[:port]`; tests and the IPv6 address `http://[fd00:ec2::254]` |
| `GLIBC_TUNABLES` | unset | Any `glibc.malloc.*` entry turns off the server's malloc pins (`mmap_threshold` 32 MiB, `trim_threshold` 64 MiB, `arena_max` 2; Linux glibc, no `mimalloc`) |
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
| `OXIMG_LINEAR_SHRINK` | `1` | `0` = full-size decode for every reduction |
| `OXIMG_JPEG_PROGRESSIVE` | `1` | `0` = sequential jpegli (SOF1, extended sequential) at every size |
| `OXIMG_JPEG_SEQUENTIAL_PIXELS` | `12000` | Outputs up to this many pixels are sequential; `0` = always progressive |
| `OXIMG_JPEG_STREAMING_PIXELS` | `16000000` | Outputs above this many pixels are sequential on the standard Huffman tables, encoded row by row; overrides the two above; `0` = every output |
| `OXIMG_FLATTEN_BG` | `ffffff` | Alpha→JPEG background |
| `OXIMG_PNG_EFFORT` | path-dependent | `fastest`/`fast`/`balanced`/`high`, or zlib-style `0`–`9` |
| `OXIMG_PNG_QUANTIZE` | `0` | `1` palette-quantizes opaque PNG |
| `OXIMG_PNG_QUANTIZE_COLORS` | `256` | Palette size 2–256 |
| `OXIMG_WEBP_QUALITY` | `75` | |
| `OXIMG_WEBP_EFFORT` | `2` | libwebp `method` |
| `OXIMG_WEBP_DECODE_THREADS` | `1` | `0` disables 2-thread decode |
| `OXIMG_AVIF_QUALITY` | `55` | libavif semantics |
| `OXIMG_AVIF_ALPHA_QUALITY` | color quality | |
| `OXIMG_AVIF_SPEED` | `8` | SVT preset |
| `OXIMG_AVIF_DECODE_THREADS` | arch-dependent | 2 on x86-64, 1 elsewhere |
| `OXIMG_MAX_SOURCE_BYTES` | 64 MiB | HTTP/GCS/S3 download buffer cap → 413 (local `process_path` is not buffered) |
| `OXIMG_MAX_SRC_PIXELS` | 64e6 | Header-parsed `w*h` cap → 413 |
| `OXIMG_MAX_DECODED_BYTES` | unset | Estimated decode allocation → 413 |
| `OXIMG_LOG_DECODED_BYTES_ABOVE` | unset | Name expensive decodes; still serve |
| `OXIMG_UPSTREAM_CONNECT_TIMEOUT` | `5` | Seconds |
| `OXIMG_UPSTREAM_TIMEOUT` | `30` | Whole fetch including credentials, retry waits and body; timeout → 504 |
| `OXIMG_OVERLAP` | `auto` | Fuse JPEG decode with resize+encode |
| `OXIMG_GIF_ANIMATION` | `1` | `0` = still first frame |
| `OXIMG_MAX_ANIM_FRAMES` | `200` | Over → still 200, not 413 |
| `OXIMG_MAX_ANIM_WORK` | `8e6` | encoded frames × post-resize area |
| `OXIMG_ANIM_FRAME_STEP` | `1` | Encode every Nth frame |

Adding a pipeline knob: field on `Config` **and** `Resolved` if it has
a per-call override, README row, `KNOBS` entry, fail-closed arm in
`validate()`, a test. Adding a server-only knob: `main.rs` + README +
this table.
