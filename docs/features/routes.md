# Routes

## Positional

`GET /resize/{w}/{h}/{*file}`

- `{file}` may span directories. `.` / `..` / empty / `\?/#` / control
  bytes → 400. A local path that escapes `IMAGES_DIR` → 404.
- `0` on one axis is unconstrained (`/resize/750/0/…` is width-only).
  Both axes zero → 400. Each axis 1–8192.
- Optional `@{fmt}` suffix on the filename (`jpg`/`jpeg`/`png`/`webp`/
  `avif`). Exact token only: `photo@2x.jpg` is a filename. `@gif` and
  `@jxl` → 400; unknown `@bogus` falls through as a filename → 404.
- Signed form: `GET /{sig}/resize/{w}/{h}/{*file}` when `OXIMG_KEY` and
  `OXIMG_SALT` are set. Unsigned → 403.

```sh
oximg-ctl get /resize/100/100/photo.jpg --expect 200
oximg-ctl get /resize/750/0/photo.jpg
oximg-ctl get /resize/100/100/photo.jpg@webp
oximg-ctl sign /resize/100/100/photo.jpg --key "$OXIMG_KEY" --salt "$OXIMG_SALT"
```

## Options (Cloudflare Images grammar)

Mounted only when `OXIMG_OPTIONS_PREFIX` is set (e.g. `/image`,
`/cdn-cgi/image`). Colliding with `/health`, `/metrics`, `/resize` is
fatal at boot.

`GET {prefix}/{options}/{*file}`

- `options` is `key=value,key=value`. Keys: `width`, `height` (1–8192,
  at least one required), `quality` (1–100), `format`
  (`jpeg|png|webp|avif|auto`). Unknown or duplicate keys → 400 naming
  the key (Cloudflare silently ignores; a dropped `fit=cover` would
  change pixels, so fail-closed is deliberate).
- Filename is literal: no `@{fmt}` on this route. `format=` owns the
  choice; absent/`auto` = same negotiation as a bare positional URL.
- Signed form: `GET /{sig}{prefix}/{options}/{*file}`. The signed
  material is the percent-decoded path, raw option order included.

```sh
oximg-ctl --env OXIMG_OPTIONS_PREFIX=/image \
  get /image/width=100,quality=80/photo.jpg
```

## Always on / opt-in

| Path | When | Notes |
|---|---|---|
| `GET /health` | always | body `ok`; used by `oximg-ctl` spawn |
| `GET /metrics` | `OXIMG_METRICS=1` | Prometheus text; **outside** the signing scheme |
| `OPTIONS` on image routes | always | 204, `Allow: GET, HEAD, OPTIONS`. Not signature-checked. oximg does not emit CORS headers |

## Precedence (output format)

explicit `@{fmt}` / `format=` > `Accept` negotiation (`OXIMG_AUTO_FORMAT`) > source format (GIF → WebP).

Negotiation off (default): no `Vary`. Negotiation on: `Vary: Accept` on every 200, including explicit-format responses.

## Signing

imgproxy-style: `base64url(HMAC-SHA256(key, salt || path))` over the
percent-decoded path, unpadded. One signature covers every encoding of
the same source. Vectors in `tests/server.rs` (`signing_gate`) and
`rubygem/oximg-rails/test/server_signer_test.rb` — `oximg-ctl sign`
must match them.
