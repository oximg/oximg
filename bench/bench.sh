#!/usr/bin/env bash
# Head-to-head benchmark: oximg vs imgproxy (Go/libvips)
# Methodology follows https://gist.github.com/DarthSim/9d971d2859f3714a29cf8ce094b3fc55:
# ab-load the same large image fit into 500x500; measure RPS / latency /
# peak memory / output size.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
N="${N:-400}"
C="${C:-8}"
IMG="${IMG:-test-large.jpg}"

RS_URL="http://127.0.0.1:8081/resize/500/500/${IMG}"
GO_URL="http://127.0.0.1:8082/insecure/resize:fit:500:500/plain/local:///${IMG}"

sample_rss() { # $1=pid $2=outfile - sample RSS(KB) every 0.2s until pid exits or is killed
  : > "$2"
  while kill -0 "$1" 2>/dev/null; do
    ps -o rss= -p "$1" >> "$2" 2>/dev/null || break
    sleep 0.2
  done
}

run_one() { # $1=name $2=url $3=pid
  local name="$1" url="$2" pid="$3"
  local rss_file="/tmp/bench-rss-${name}.txt"
  local ab_file="/tmp/bench-ab-${name}.txt"

  # warm-up
  for _ in $(seq 1 20); do curl -sf -o /dev/null "$url"; done

  sample_rss "$pid" "$rss_file" &
  local sampler=$!
  ab -n "$N" -c "$C" "$url" > "$ab_file" 2>&1
  kill "$sampler" 2>/dev/null || true
  wait "$sampler" 2>/dev/null || true

  local rps p50 p95 mean bytes peak_mb
  rps=$(awk '/Requests per second/ {print $4}' "$ab_file")
  mean=$(awk '/Time per request.*mean\)/ {print $4}' "$ab_file")
  p50=$(awk '$1=="50%" {print $2}' "$ab_file")
  p95=$(awk '$1=="95%" {print $2}' "$ab_file")
  bytes=$(curl -sf -o /dev/null -w '%{size_download}' "$url")
  peak_mb=$(sort -n "$rss_file" | tail -1 | awk '{printf "%.0f", $1/1024}')

  printf "%-14s %8s req/s  mean %7s ms  p50 %5s ms  p95 %5s ms  peak RSS %5s MB  out %6s B\n" \
    "$name" "$rps" "$mean" "$p50" "$p95" "$peak_mb" "$bytes"
}

echo "=== N=$N C=$C IMG=$IMG ==="
run_one "$@"
