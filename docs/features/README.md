# Feature map

Compressed index of what oximg does, how a caller reaches it, and how
an agent proves a change. The [README](../../README.md) is the long
form; this directory is the token-cheap projection. Drive the binary
with [`oximg-ctl`](../../src/bin/oximg-ctl.rs) — JSON on stdout, real
`oximg` underneath, fixtures under `tests/fixtures/`.

| File | What it catalogs |
|---|---|
| [paths.md](paths.md) | Three caller paths: HTTP, CLI, library |
| [routes.md](routes.md) | URL grammars, signing, CORS, health, metrics |
| [formats.md](formats.md) | Decode/encode matrix, `@{fmt}`, GIF→WebP |
| [invariants.md](invariants.md) | Fit, color, orientation, ICC, animation budgets |
| [knobs.md](knobs.md) | `OXIMG_*` / `QUALITY` / `PRESET` inventory |
| [errors.md](errors.md) | HTTP statuses and `ErrorKind` |

`cargo test --release` is necessary and not sufficient. A change that
touches HTTP, pixels, or a knob is verified only when `oximg-ctl`
(or the suites that spawn the same binary) has run the compiled
artifact and the JSON proof matches the invariant.

Maintain this map when a route, knob, format, or status is added or
removed. `src/config.rs` pins pipeline knobs to the README; this map
must not drift from either.
