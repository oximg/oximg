# How to get there

Three surfaces, one pipeline. Prefer the surface the change actually
ships on; the others are cross-checks.

## HTTP (the server)

```sh
oximg-ctl get /resize/100/100/photo.jpg
oximg-ctl get /resize/100/100/photo.jpg@webp --write /tmp/out.webp
oximg-ctl --env OXIMG_OPTIONS_PREFIX=/image get /image/width=100/photo.jpg
oximg-ctl matrix --source photo.jpg --box 100x100
```

Auto-spawns `oximg` on `PORT=0` with `IMAGES_DIR=tests/fixtures`
unless `--base URL` points at an already-running server. Proof is
`status`, `content_type`, `probe.{width,height}`, `sha256`.

## CLI (one-shot, no HTTP)

```sh
oximg-ctl probe tests/fixtures/photo.jpg
oximg-ctl resize tests/fixtures/photo.jpg 100 100 --out /tmp/out.jpg
oximg-ctl resize tests/fixtures/anim.gif 100 100 -f webp --out /tmp/out.webp
```

`resize` shells out to `oximg resize` (same argv a user types). Usage
errors exit 2; processing failures exit 1. `0` on an axis is
unconstrained; `0 0` re-encodes at native size (CLI only — the server
refuses `0/0`).

## Library (`oximg::pipeline`)

```sh
cargo run --release --example thumbnail -- tests/fixtures/photo.jpg 100 100 /tmp/out.jpg
cargo run --release --example probe -- tests/fixtures/photo.webp
```

`process` / `process_path` take `Params` and return `(bytes, ImageFormat)`.
`probe` is header-only stored size; `probe_animation` reports frames
for GIF/WebP. Embedders set `default-features = false` to drop the
HTTP stack. Failures are `pipeline::Error`; match `kind()`, wildcard
arm required (`ErrorKind` is `#[non_exhaustive]`).

A library-only change still needs a binary-level cell if the HTTP or
CLI contract can see it (status, content-type, CLI exit code).
