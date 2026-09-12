# Errors

Status follows **fault**, not convenience. CDNs retry 5xx and cache
4xx; misfiling a client error as upstream both inflates the 5xx rate
and turns a crawler into origin load.

## HTTP

| Status | When | Body |
|---|---|---|
| 200 | success, including animation that degraded to a still | image bytes |
| 204 | `OPTIONS` preflight | empty, `Allow: GET, HEAD, OPTIONS` |
| 400 | bad dimensions (`0/0`, >8192); path traversal syntax; `@gif`/`@jxl`; unknown options-route key; source key the origin will not serve (400/414, over-length) | names the cause when it is the client's grammar |
| 403 | signing on, missing or wrong signature | |
| 404 | missing object; path escaping `IMAGES_DIR`; unknown `@bogus` (filename) | `image not found` |
| 405 | method other than GET/HEAD/OPTIONS | |
| 413 | `OXIMG_MAX_SOURCE_BYTES` / `MAX_SRC_PIXELS` / `MAX_DECODED_BYTES` | generic; which limit is on stderr |
| 422 | undecodable bytes, or a capability this build lacks (AVIF out without the feature) | top-level message, safe to echo |
| 500 | unreadable local source, encoder/internal fault, worker panic | generic; chain on stderr |
| 502 | upstream broken (connect/reset/5xx) | generic |
| 504 | upstream slow (`OXIMG_UPSTREAM_*` deadline) | generic |

```sh
oximg-ctl get /resize/0/0/photo.jpg --expect 400
oximg-ctl get /resize/100/100/missing.jpg --expect 404
oximg-ctl get /resize/100/100/photo.jpg@gif --expect 400
oximg-ctl get /resize/100/100/photo.jpg@bogus --expect 404
```

## Library (`pipeline::ErrorKind`)

The server's match in `error_response` is the status table above.

| Kind | HTTP |
|---|---|
| `SourceNotFound` | 404 |
| `SourceRejected` | 400 |
| `SourceTooLarge` | 413 |
| `SourceUnreadable` | 500 |
| `Undecodable` | 422 |
| `Upstream` | 502 |
| `UpstreamTimeout` | 504 |
| `Internal` | 500 |
| unknown (`#[non_exhaustive]`) | 500 |

CLI: usage → exit 2; processing failure → exit 1. `oximg-ctl` usage →
exit 2 with JSON `{ok:false, hint}`; a failed proof → exit 1 with the
same object plus whatever was observed (`status`, `probe`, …).
