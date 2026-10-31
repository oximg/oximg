#!/usr/bin/env bash
# The local gate, in two tiers. Progress and failing logs go to stderr;
# stdout is one JSON object, also written to target/check/last.json so a
# hook can tell whether HEAD passed.
#
#   scripts/check.sh quick [-- <cargo test args>]   # the edit loop
#   scripts/check.sh full                           # before a PR
#
# quick: fmt, the fuzz lockfile, clippy, and tests on the `agent` profile (release codegen
# without fat LTO). Args after `--` go to `cargo test`, e.g.
# `-- --test ctl`.
# full: what CI's amd64 test job runs, on the release profile. The AVIF
# steps run only when pkg-config finds SVT-AV1 >= 4.1 and dav1d;
# otherwise they are reported as skipped. arm64, MSRV, coverage, the
# Ruby gems, and cargo deny stay CI's.
#
# Exit 0 when every step that ran passed, 1 on a failed step, 2 on usage.
# Bash 3.2 compatible (stock macOS).
set -uo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

mode=${1:-}
case "$mode" in
  quick | full) shift ;;
  *)
    echo "usage: scripts/check.sh quick|full [-- <cargo test args>]" >&2
    exit 2
    ;;
esac
test_args=()
if [[ "${1:-}" == "--" ]]; then
  shift
  test_args=("$@")
elif [[ $# -gt 0 ]]; then
  echo "unexpected argument: $1 (cargo test args go after --)" >&2
  exit 2
fi
if [[ "$mode" == full && ${#test_args[@]} -gt 0 ]]; then
  echo "full runs the whole suite; pass test args to quick" >&2
  exit 2
fi

out_dir="$root/target/check"
mkdir -p "$out_dir"

avif=false
if command -v pkg-config >/dev/null 2>&1 &&
  pkg-config --atleast-version=4.1 SvtAv1Enc 2>/dev/null &&
  pkg-config --exists dav1d 2>/dev/null; then
  avif=true
fi

ok=true
steps_json=""
started=$(date +%s)

# Untracked files count: cargo compiles a new tests/*.rs or a `mod`-ed
# src file whether or not it is committed, so a pass with one present
# says nothing about HEAD alone.
tree_state() {
  git rev-parse HEAD 2>/dev/null || echo unknown
  git status --porcelain 2>/dev/null
}
# Sampled before the first step and again after the last: a commit or
# an edit made in another terminal mid-run must not let this result
# vouch for code no step saw.
before=$(tree_state)

# step NAME NEEDS_AVIF CMD...
step() {
  local name=$1 needs_avif=$2
  shift 2
  local entry
  if [[ "$ok" != true ]]; then
    entry="{\"name\": \"$name\", \"status\": \"not_run\"}"
  elif [[ "$needs_avif" == avif && "$avif" != true ]]; then
    entry="{\"name\": \"$name\", \"status\": \"skipped\", \"reason\": \"no SVT-AV1 >= 4.1 / dav1d via pkg-config\"}"
    echo "check: $name skipped (no AVIF toolchain)" >&2
  else
    local log="$out_dir/$name.log" t0 secs
    t0=$(date +%s)
    echo "check: $name: $*" >&2
    if "$@" >"$log" 2>&1; then
      secs=$(($(date +%s) - t0))
      entry="{\"name\": \"$name\", \"status\": \"passed\", \"secs\": $secs}"
    else
      secs=$(($(date +%s) - t0))
      ok=false
      entry="{\"name\": \"$name\", \"status\": \"failed\", \"secs\": $secs, \"log\": \"target/check/$name.log\"}"
      echo "check: $name FAILED after ${secs}s; last lines of target/check/$name.log:" >&2
      tail -n 40 "$log" >&2
    fi
  fi
  steps_json="${steps_json:+$steps_json, }$entry"
}

# CI's "Examples smoke": run the example CLIs once, not just compile them.
examples_smoke() {
  cargo run --release --example probe -- tests/fixtures/photo.webp &&
    cargo run --release --example thumbnail -- tests/fixtures/photo.jpg 64 64 "$out_dir/t.jpg"
}
examples_smoke_avif() {
  cargo run --release --example transcode --features avif -- tests/fixtures/photo.jpg 64 64 avif "$out_dir/t.avif" &&
    cargo run --release --example qcli --features avif -- transcode tests/fixtures/photo.jpg 64 64 webp "$out_dir/q.webp"
}

step fmt any cargo fmt --check
# fuzz/ is its own crate with its own lockfile, which no other step
# builds: without this a dependency change leaves it stale (#148).
step fuzz-lock any cargo metadata --locked --format-version 1 --manifest-path fuzz/Cargo.toml
if [[ "$mode" == quick ]]; then
  step clippy any cargo clippy --profile agent --all-targets -- -D warnings
  step test any cargo test --profile agent ${test_args[@]+"${test_args[@]}"}
else
  step clippy any cargo clippy --release --all-targets -- -D warnings
  step clippy-avif avif cargo clippy --release --all-targets --features avif -- -D warnings
  step clippy-bench-tools avif cargo clippy --release --examples --features avif,bench-internals -- -D warnings
  step clippy-library-only avif cargo clippy --release --no-default-features --features avif -- -D warnings
  step test any cargo test --release
  step test-avif avif cargo test --release --features avif
  step build-all-targets avif cargo build --release --all-targets --features avif
  step examples-smoke any examples_smoke
  step examples-smoke-avif avif examples_smoke_avif
fi

head=${before%%$'\n'*}
dirty=false
# Dirty at the start, or anything different at the end (HEAD moved,
# files changed): the steps did not check one clean HEAD.
[[ "$before" != "$head" || "$(tree_state)" != "$before" ]] && dirty=true
filtered=false
[[ ${#test_args[@]} -gt 0 ]] && filtered=true

json="{\"ok\": $ok, \"mode\": \"$mode\", \"head\": \"$head\", \"dirty\": $dirty, \"filtered\": $filtered, \"avif\": $avif, \"secs\": $(($(date +%s) - started)), \"steps\": [$steps_json]}"
echo "$json" >"$out_dir/last.json"
echo "$json"
[[ "$ok" == true ]]
