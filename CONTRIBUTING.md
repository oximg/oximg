# Contributing

Small fixes can go straight to a PR. For anything larger, open an
issue first — the docs in this repo carry a lot of intent, and a short
conversation up front usually saves a rewrite.

## Build prerequisites

A stable Rust toolchain and a C/C++ toolchain, plus two tools the
**default** features already require:

- **cmake** — `jpegli-sys` builds the jpegli C++ encoder from source.
- **nasm** — `mozjpeg-sys` assembles mozjpeg's SIMD.

So `cargo build --release` fails without them even with AVIF off.
Debian/Ubuntu: `sudo apt-get install cmake nasm`. macOS:
`brew install cmake nasm`.

The `avif` feature additionally needs SVT-AV1 >= 4.1 and dav1d, both
visible to pkg-config (Debian/Ubuntu ships `libdav1d-dev`; SVT-AV1
4.1 is usually too new to be packaged, so CI and the Dockerfile build
a pinned post-4.1 revision from source — the `SVT_AV1_REV` in
[.github/workflows/ci.yml](.github/workflows/ci.yml); build that
revision to match CI exactly). Everything else builds from crates.io
with no system libraries.

The MSRV is **Rust 1.90** (`rust-version` in Cargo.toml, bounded by
pic-scale and enforced by the CI `msrv` job).

## Feature map

| Build | What it is |
|---|---|
| default (= `server`) | The `oximg` binary and the full HTTP stack |
| `--no-default-features` | Library only — the `pipeline` API without axum/tokio/reqwest |
| `--features avif` | AVIF encode (SVT-AV1) and decode (dav1d) |
| `--features bench-internals` | Re-exposes internals for the bench tools under `bench/tools/` (compiled as examples; not public API) |

## Running the tests

```sh
cargo test --release                    # default features
cargo test --release --features avif    # if the AVIF toolchain is installed
```

What the suites assume — and deliberately don't:

- **No network.** The remote-source tests (`tests/remote_api.rs`)
  spin up local fake origins and a fake GCS metadata server; nothing
  reaches the internet.
- The server suites spawn the compiled binary on an OS-assigned port
  (`PORT=0`), so `cargo test` builds the binary as a side effect and
  never collides on a fixed port.
- All fixtures are committed under `tests/fixtures/` — no downloads,
  no generation step.
- Library-level tests share one process-wide config (a `OnceLock`),
  so env-dependent tests funnel through a single `init()` — see the
  note at the top of `tests/remote_api.rs` before adding one.

## What CI checks before merge

The [CI workflow](.github/workflows/ci.yml) gates every PR on:

- `cargo fmt --check`
- clippy with `-D warnings` in four feature configurations:
  `--all-targets` (default), `--all-targets --features avif`,
  `--examples --features avif,bench-internals` (the bench tools), and
  `--no-default-features --features avif` (the library must build
  without the HTTP stack)
- `cargo test --release` with default features and with `avif` — on
  **both amd64 and arm64** (the NEON kernels are production code)
- a runtime smoke of the examples
- **MSRV**: `cargo check --release --all-targets` on Rust 1.90
- **Coverage**: `cargo llvm-cov --release --features avif
  --fail-under-lines 80` — a collapse detector, not a target (the
  suite currently sits around 86%)
- the **Ruby gem** suites on Ruby 3.1 and 3.4, plus the same suites
  against a musl build on Alpine
- **License compliance**
  ([compliance.yml](.github/workflows/compliance.yml)):
  `cargo deny check`, and `THIRD-PARTY-LICENSES.md` must match the
  dependency tree — after adding or bumping a dependency, regenerate
  it:

  ```sh
  cargo about generate about.hbs -o THIRD-PARTY-LICENSES.md
  ```

The short local loop that catches most of it:

```sh
cargo fmt --check
cargo clippy --release --all-targets -- -D warnings
cargo test --release
```

## The Ruby gems

`rubygem/oximg` (drives the binary) and `rubygem/oximg-rails` (builds
and signs URLs for the server) — see
[rubygem/README.md](rubygem/README.md) for the packaging story. Both
suites run against a real oximg build rather than a stub — the CLI
grammar and the signing scheme are the contracts between the two
languages, and a stub would agree with whatever the gem believes:

```sh
cargo build    # debug is fine; the tests find target/{debug,release}/oximg
cd rubygem/oximg && bundle install && bundle exec rake test
cd ../oximg-rails && bundle install && bundle exec rake test
```

Ruby >= 3.1 (the declared floor; CI also runs the current release).
Without a binary the integration tests skip rather than fail, so the
unit halves stay runnable on a machine with no Rust toolchain;
`OXIMG_BIN` points the gem at any other binary.

## Fuzzing

The hand-written parser layer has two `cargo fuzz` targets under
`fuzz/` (`options_parse`, `probe`), smoke-fuzzed by CI on every
`main` push and dug into weekly. Locally (nightly toolchain):

```sh
cargo +nightly fuzz run options_parse
```

A crashing input uploaded by a red CI run reproduces with
`cargo fuzz run <target> <artifact>`.

## Changelog

User-visible changes get an entry under `[Unreleased]` in
[CHANGELOG.md](CHANGELOG.md) (Keep a Changelog format);
internal-only changes don't.
