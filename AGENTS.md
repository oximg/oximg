# AGENTS.md

oximg is a Rust image-compression engine with three caller paths: an
HTTP server (`oximg`), CLI one-shots (`oximg resize` / `oximg probe`),
and a library (`oximg::pipeline`). `oximg-ctl` is the JSON control
plane agents drive it with. Instructions here apply to every coding
agent; [`CLAUDE.md`](CLAUDE.md) loads this file. Humans:
[CONTRIBUTING.md](CONTRIBUTING.md) is the long form.

## Done means proved

A green `scripts/check.sh` is necessary and not sufficient. A change
that touches HTTP, pixels, CLI argv, or a knob is done when `oximg-ctl`
has run the compiled binary and its JSON matches
[`docs/features/`](docs/features/). Follow the verify skill:
[`.claude/skills/verify/SKILL.md`](.claude/skills/verify/SKILL.md). Paste
the JSON that proves it into the PR body; "tests passed" is not proof.
A new behavior also gets a `tests/ctl.rs` (or sibling suite) cell so CI
keeps the proof.

## Where things are

| Path | What |
|---|---|
| `src/main.rs` | HTTP server: router, handlers, signing, ErrorKind → status |
| `src/cli.rs` | `oximg resize` / `oximg probe` |
| `src/options.rs` | Cloudflare-style option-list grammar |
| `src/config.rs` | Every `OXIMG_*` knob, plus the tests that pin docs to code |
| `src/pipeline/` | Decode → resize → encode; `jpeg_dec.rs`, `jpegli_enc.rs`, `gif.rs`, `gcs.rs`, `linear_shrink.c` |
| `src/resize_{kernel,avx2,neon}.rs` | SIMD resize kernels (arch-neutral core + AVX2 + NEON) |
| `src/avif/`, `src/svt/`, `src/yuv.rs` | AVIF (feature `avif`): SVT-AV1 FFI, dav1d decode |
| `src/bin/oximg-ctl/` | Control plane: one module per command under `cmd/` (`get`, `matrix`, `parity`, `probe`, `resize`, `sign`, `serve`) |
| `tests/` | Integration suites; `server.rs` is ~3.7k lines — grep for the test, do not read it whole |
| `docs/features/` | Token-cheap feature map: routes, formats, knobs, errors, invariants |
| `bench/` | Benchmarks and quality sweeps; rules in `bench/METHODOLOGY.md`; `bench/platforms/` measures the deploy targets |
| `deploy/` | Per-target deploy scripts and configs (Cloud Run, Cloudflare Containers, Cloudflare Workers wasm build); guides in `docs/deploy-*.md` |
| `rubygem/`, `npm/`, `site/` | Ruby gems, npm wrapper, website (own workflow) |

## Build and check

Default features need `cmake` (jpegli) and `nasm` (mozjpeg SIMD); MSRV
is Rust 1.90. `--features avif` also needs SVT-AV1 ≥ 4.1 and dav1d
(CONTRIBUTING.md has the pinned revision).

Iterate on the `agent` profile, not `--release`. Release is fat LTO
with one codegen unit: on a 16-thread Ryzen a one-line edit costs
~100 s to relink, while `agent` (same codegen, no LTO, incremental)
relinks in seconds and runs the whole suite warm in ~5 s (measured
2026-10-14). Binaries land in `target/agent/`. Performance numbers
still come from `--release` only.

The local gate, one JSON object on stdout (also `target/check/last.json`):

```sh
scripts/check.sh quick                  # fmt, clippy, tests on `agent`
scripts/check.sh quick -- --test ctl    # args after -- go to cargo test
scripts/check.sh full                   # CI's amd64 job, on release
```

`full` adds the clippy and test variants CI runs with `--features
avif`, `--examples --features avif,bench-internals`, and
`--no-default-features --features avif` (the library must build
without the HTTP stack); without SVT-AV1/dav1d it reports those steps
as skipped. CI also runs arm64, MSRV, coverage ≥ 80%, the Ruby gems,
and `cargo deny`.

Push only commits `full` has passed on a clean tree.
`.githooks/pre-push` enforces it for Claude Code sessions, which enable
it with `git config core.hooksPath .githooks`; a branch that touches
only agent files, `site/`, or Markdown other than `README.md` and
`docs/features/` (which `src/config.rs` tests) is exempt. Never
`--no-verify`.

## Docs that CI pins to code

Adding or removing a knob, an `ErrorKind`, an HTTP status mapping, or
an `@{fmt}` token fails `src/config.rs` tests until `README.md` and the
matching `docs/features/` file agree. Update them in the same commit.
After a dependency change, regenerate the license bundle:
`cargo about generate --features avif about.hbs -o THIRD-PARTY-LICENSES.md`
(cargo-about 0.9.2, the version CI pins).

## Performance claims

Any number in a PR or doc follows `bench/METHODOLOGY.md`:
interleaved same-window A/B, never sequential before/after; release
profile only; several rounds; name the CPU. SIMD and decoder changes
need the same proof on amd64 and arm64. Byte-identical output is
proved with `bench/bytecmp.sh`, not asserted.

## Conventions

- Commits: conventional, scoped by area — `perf(jpeg): …`,
  `fix(ctl): …`, `docs(bench): …`, `test(server): …`. One topic per
  commit. Branches: `<type>/<slug>` (`perf/linear-shrink-simd`).
- User-visible changes get an entry under `[Unreleased]` in
  CHANGELOG.md; internal-only changes don't.
- Comments say *why*: a hidden constraint, a measured result, an ABI
  fact. Match the density of the file you are in.
- Do not set `panic = "abort"`: the libjpeg error handlers unwind
  through C frames (`src/panic_guard.rs`).

## Traps

- Tests are hermetic: no network, fixtures committed under
  `tests/fixtures/`. Remote-source tests fake their origins; never
  reach the internet to prove a change.
- Library tests share one process-wide config (`OnceLock`): an
  env-dependent test goes through the single `init()` — read the note
  at the top of `tests/remote_api.rs` first.
- `cargo run --bin oximg-ctl` does not build `oximg`; run
  `cargo build --profile agent` first.
- Never `killall oximg` / `pkill -f oximg`: other sessions on the host
  run their own. Tear down only the pid you started.
