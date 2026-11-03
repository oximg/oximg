---
name: verify
description: >
  Close the loop on oximg (Rust image compression: HTTP server + CLI)
  by driving the compiled binary with oximg-ctl and matching JSON to
  docs/features/. cargo test green is not done. Use when changing HTTP
  routes, CLI one-shots, formats, signing, knobs, pixels, or when asked
  to verify, prove, /verify, oximg-ctl, or whether a change actually
  works. The canonical copy for every agent; .cursor/ and .grok/ point
  here.
---

# Verify oximg

A green `scripts/check.sh` is necessary and not sufficient. A change that
touches HTTP, pixels, CLI argv, or a knob is verified only when
`oximg-ctl` has run the **compiled** artifact and the JSON matches
[docs/features/](../../../docs/features/).

This is the one drive book for every agent. The Cursor and Grok skills
(`.cursor/skills/verify-oximg/`, `.grok/skills/verify/`) are pointers
to this file — edit here, never there. The truth is `oximg-ctl` +
`docs/features/`: invariants, routes, formats, knobs, and error
classes live in that map; do not copy them here. `oximg-ctl --help` is
the CLI contract.

Primary surfaces: HTTP resize/options routes and CLI one-shots. The
library (`oximg::pipeline`) is a cross-check — a library-only change
still needs a binary-level cell if HTTP or CLI can see it.

Feature files under [features/](features/) are what to drive, not a
second invariant dump. After a route, knob, format, or status lands,
run `/maintain-verification-skill` so this map stays honest.

## Launch

Build once, then drive isolated auto-spawn sessions. Do **not** leave a
long-lived shared server running for ordinary proofs.

Prerequisites (default features already need these; the checkout will
not compile without them):

- Rust ≥ 1.90 (MSRV; `rust-version` in `Cargo.toml`)
- `cmake` (jpegli C++ encoder) and a C++ toolchain that can link `libstdc++`
- `nasm` (mozjpeg SIMD)

```sh
cargo build --profile agent           # both oximg and oximg-ctl
# cargo build --profile agent --features avif   # only when AVIF is under test
# cargo run --bin oximg-ctl does not build oximg
```

The `agent` profile is release codegen without fat LTO: a one-line
edit relinks in seconds, not ~100 s, and its output matched release
byte for byte on the fixture matrix (`oximg-ctl parity`): amd64
with and without avif, and aarch64 with default features only, under
QEMU — AVIF on arm64 is unchecked. When the risk is the
codegen itself — SIMD, `unsafe`, FFI — also drive the release binary
(`cargo build --release`, `CTL=./target/release/oximg-ctl`,
`OXIMG_PROFILE=release` for doctor), and on arm64 too.

Readiness: both binaries exist and doctor exits 0.

```sh
test -x ./target/agent/oximg
test -x ./target/agent/oximg-ctl
.claude/skills/verify/scripts/doctor.sh
```

Each `oximg-ctl get` / `matrix` auto-spawns `oximg` on `PORT=0`,
`OXIMG_BIND=127.0.0.1`, `IMAGES_DIR=tests/fixtures` (override with
`--images-dir`), and `OXIMG_WORKERS=1` if unset. Ready when stderr
prints `oximg listening on :N` **and** `GET /health` returns 200 (ctl
waits up to 15s; it does not inspect the body). oximg itself answers
`/health` with body `ok`. The child is reaped when that ctl process
exits — no extra teardown.

Prefer that path over `serve`. Auto-spawn also **strips inherited**
`OXIMG_KEY`, `OXIMG_SALT`, and `OXIMG_SOURCE_BASE_URL` so unsigned
fixture `get`/`matrix` do not 403. Opt into a signed spawn with
`--env OXIMG_KEY=<hex>` and `--env OXIMG_SALT=<hex>` (both non-empty;
empty is unset). `--base URL` talks to a foreign tree (no local
fixture oracle).

If you truly need a held process (many `--base` calls against one
listener), bind loopback and capture the JSON:

```sh
./target/agent/oximg-ctl --env OXIMG_BIND=127.0.0.1 --pretty serve
# one JSON object, then wait: ok, pid (the oximg child), port, base, bin, images_dir
```

Readiness for `serve`: that JSON plus `GET $base/health` → 200 `ok`.
Teardown: SIGTERM the **wrapper** (`oximg-ctl`) pid you started — it
forwards to the child. Never `killall oximg`.

Hermetic: no network. Fixtures are committed under `tests/fixtures/`.
Remote-source tests spin local fake origins; do not hit the internet
to prove a change.

## Doctor

One read-only check. Does not spawn, bind a port, or kill anything.

```sh
.claude/skills/verify/scripts/doctor.sh
```

Pass (`ok: true`, exit 0) means: `cmake` and `nasm` are on `PATH`,
`./target/agent/oximg` and `./target/agent/oximg-ctl` (or
`target/$OXIMG_PROFILE/`) exist,
report the `Cargo.toml` package version (`oximg <version>` /
`oximg-ctl <version>`), and are newer than every file under `src/` and
`build.rs` — a stale binary is proof for other code, and between two
releases the version alone cannot tell. `tests/fixtures/` is a
directory. `OXIMG_VERIFY_PID` unset →
`spawned: none` (the expected state after auto-spawn). If set, that
pid must be alive (Unix `ps`/`kill`, not `/proc`), owned by this uid,
and `comm` must be `oximg` or `oximg-ctl`; otherwise `ok: false` and
`spawned` is `invalid` / `dead` / `foreign` / `wrong-exe`. It never
greps or kills by process name.

Capture stdout to evidence (it is JSON).

## Drive

From the repo root, after Launch. stdout is **one JSON object**
(`--help` / `--version` are plain text). Exit 0 on success, 1 when a
command ran and failed its proof, 2 for usage (`ok: false`, `hint`).

Prefer stable handles: URL paths, `--expect`, JSON fields
(`status`, `content_type`, `probe.width` / `probe.height`,
`probe.animation.frames`, `sha256`, `signature`). Do not scrape
stderr timings or pixel bytes.

```sh
CTL=./target/agent/oximg-ctl

# HTTP positional resize (auto-spawn, then reap)
$CTL get /resize/100/100/photo.jpg --expect 200
$CTL get /resize/750/0/photo.jpg --expect 200
$CTL get /resize/100/100/tiny.jpg --expect 200          # stays 40×30
$CTL get /resize/0/0/photo.jpg --expect 400
$CTL get /resize/100/100/missing.jpg --expect 404

# Fixture × box × format, including 400/404 negatives
$CTL matrix --source photo.jpg --box 100x100

# @{fmt} and GIF→WebP (GIF has no encoder)
$CTL get /resize/100/100/photo.jpg@webp --expect 200
$CTL get /resize/100/100/still.gif --expect 200         # image/webp
$CTL get /resize/100/100/anim.gif --expect 200          # animated webp
$CTL get /resize/100/100/photo.jpg@gif --expect 400
$CTL get /resize/100/100/photo.jpg@bogus --expect 404

# Cloudflare-style options route (not mounted unless the env is set)
$CTL --env OXIMG_OPTIONS_PREFIX=/image \
  get /image/width=100,quality=80/photo.jpg --expect 200

# CLI one-shot: resize shells out to `oximg resize` (user argv).
# probe is in-process library — pair with `oximg probe` / tests/cli.rs
# when the CLI argv contract is what changed.
$CTL probe tests/fixtures/photo.jpg
$CTL probe tests/fixtures/anim.gif
$CTL resize tests/fixtures/photo.jpg 100 100 --out /tmp/oximg-verify-out.jpg
$CTL resize tests/fixtures/anim.gif 100 100 -f webp --out /tmp/oximg-verify-out.webp
# Memory: the decoded-bytes estimate beside the child's peak RSS
# (`decoded_bytes.total` vs `peak_rss_bytes`).
$CTL --env OXIMG_LOG_DECODED_BYTES_ABOVE=1 resize tests/fixtures/photo.jpg 0 0
# Strip: shells out to `oximg strip … --json`; `reported_matches` proves
# the child's JSON is the library's answer. A tagged JPEG for
# --auto-rotate is photo_with_orientation() in tests/ctl.rs.
$CTL strip tests/fixtures/animated.webp
$CTL strip /tmp/tagged.jpg --auto-rotate   # rotated | not_rotated{code,message}

# Signing: same HMAC as tests/server.rs `signing_gate` / tests/ctl.rs
KEY=deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef
SALT=cafebabecafebabecafebabecafebabecafebabecafebabecafebabecafebabe
$CTL sign /resize/100/100/photo.jpg --key "$KEY" --salt "$SALT"
# unsigned → 403 once both keys are on the spawn; signed path → 200
$CTL --env OXIMG_KEY="$KEY" --env OXIMG_SALT="$SALT" \
  get /resize/100/100/photo.jpg --expect 403
$CTL --env OXIMG_KEY="$KEY" --env OXIMG_SALT="$SALT" \
  get /t-jKRoyvzhs4dEBnGGBUS_t6Uh_HE6WysfGYvs8UaTo/resize/100/100/photo.jpg \
  --expect 200
```

`--pretty` indents JSON. `--write PATH` saves an HTTP body (side
effect). `--dry-run` prints the plan without spawning. `--bin PATH`
selects the `oximg` binary. Without `--bin`, `resolve_bin` uses
`OXIMG_BIN` if set and non-empty, else a sibling `oximg` next to the
running `oximg-ctl`, else `PATH` (`oximg`).

Which layer to drive (do not run every layer every time):

| Touch | Proof |
|---|---|
| Any PR | `scripts/check.sh full` (runs the AVIF steps when that toolchain is present) **and** ctl JSON for the changed surface |
| Routes, `@{fmt}`, box, errors | `get` and/or `matrix` covering the change |
| Signing | `sign` vs the vectors in [routes.md](../../../docs/features/routes.md) / `tests/ctl.rs`; then signed `get` with `--env OXIMG_KEY` / `OXIMG_SALT` |
| CLI / one-shot | `resize` (real `oximg resize`); `oximg probe` or `tests/cli.rs` for probe/argv |
| Decode / resize / encoder / default knobs | ctl JSON matching [docs/features/](../../../docs/features/) — QUALITY ([bench/quality/QUALITY.md](../../../bench/quality/QUALITY.md)) is an extra fidelity check, not a substitute |
| Hot path latency/throughput | [bench/METHODOLOGY.md](../../../bench/METHODOLOGY.md) — interleaved A/B, never sequential, release profile only |
| SIMD | the same proof on **amd64 and arm64** |

A new behavior also needs a `tests/ctl.rs` (or sibling suite) cell so
CI keeps the proof. Running ctl once locally is not a substitute.

Pick a feature file under [features/](features/) and run its
**Driving it with oximg-ctl** block. Link out to `docs/features/` for
invariants — do not duplicate that tree here.

## Evidence

Write under `target/verify-evidence/` (gitignored with the rest of
`target/`). Evidence is never committed: a JSON file in the tree goes
stale with the next release, while the PR body is pinned to the commit
it proves. Paste the JSON that carries the proof into the PR.

For each driven feature, capture:

1. **Action** — the exact `oximg-ctl` argv (record it in the filename
   or a sidecar `.cmd` line).
2. **Resulting state** — ctl stdout JSON (`--pretty` is fine).
3. **Side effects** — `--write` / `--out` bytes only when the proof
   needs the file; JSON `sha256` + `probe` already cover most cells.
4. **Doctor** — `doctor.sh` stdout from this session.

Proof standards:

- Real user path: HTTP `GET` the route a client would hit, or CLI
  argv a user would type (`oximg-ctl resize` shells out to that).
- `ok: true` and process exit 0. HTTP: `status` (and `--expect` when
  you care), `content_type`, `probe.width` / `probe.height` on 200
  image bodies. `probe.error` on a 200 image is a failed proof.
- Non-2xx is a successful **observation** unless `--expect` disagrees.
- Signing: `signature` matches the documented vector; signed `get` is
  200, unsigned is 403 while keys are set.
- Mocks only at production boundaries (this repo's tests already fake
  origins locally). Do not stub `oximg` itself.
- Show the JSON (or the failing object) in the PR / reply. "tests
  passed" is not proof.

Suggested names: `target/verify-evidence/<feature>.json` (ctl
stdout), `target/verify-evidence/doctor.json`. The JSON is the proof;
binary dumps (`--write`) only when it needs the bytes.

## Cleanup

Tear down only what this session started.

- Auto-spawn (`get` / `matrix` without `--base`): already gone when
  ctl exits. Leave `OXIMG_VERIFY_PID` unset and confirm doctor still
  reports `spawned: none`.
- `serve`: SIGTERM the wrapper pid you launched (the `oximg-ctl`
  process). It forwards to the child in the ready JSON's `pid`. Wait
  for the wrapper to exit. If you exported `OXIMG_VERIFY_PID`, unset
  it after the child is reaped (a leftover value makes doctor report
  `dead`, not `none`).
- Temp `--out` files you wrote may be removed; keep
  `target/verify-evidence/` until the PR body has its JSON.
- Never `killall oximg` / `pkill -f oximg` / kill-by-process-name.
- Never kill a pid you did not start (including a `--base` server).

## Helpers

| Script | When |
|---|---|
| [scripts/doctor.sh](scripts/doctor.sh) | Launch readiness and post-cleanup check (read-only, JSON on stdout) |

Invoke it from the repo root as shown in Doctor. It must stay
executable (`chmod +x`). Do not add a second harness — `oximg-ctl`
and `tests/ctl.rs` already are the control plane.
