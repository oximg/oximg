#!/usr/bin/env bash
# Quick native benchmark: build, start the server, measure throughput and
# peak memory. No Docker required.
#
# Requirements: cargo, ab (Apache bench), ImageMagick (magick), curl.
# Usage: bench/native.sh            # defaults
#        PRESET=fast bench/native.sh
#        N=3200 C=8 bench/native.sh
set -euo pipefail
cd "$(dirname "$0")/.."

N=${N:-1600}
C=${C:-$(nproc)}
PORT=${PORT:-8091}
PRESET=${PRESET:-jpegli}

cargo build --release

mkdir -p images
[ -f images/test-medium.jpg ] ||
  magick -size 2000x1333 plasma:fractal -colorspace sRGB -quality 92 images/test-medium.jpg
[ -f images/test-large.jpg ] ||
  magick -size 7360x4912 plasma:fractal -colorspace sRGB -quality 92 images/test-large.jpg

PRESET=$PRESET IMAGES_DIR=./images PORT=$PORT ./target/release/oximg &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
sleep 0.5
curl -sf -o /dev/null --retry 10 --retry-connrefused "http://127.0.0.1:$PORT/health"

run() { # label file n
  local label=$1 file=$2 n=$3
  for w in $(seq 485 500); do
    curl -sf -o /dev/null "http://127.0.0.1:$PORT/resize/$w/500/$file"
  done
  # diverse: 16 workers on 16 distinct widths — every request is real work
  local pids=() t0 t1
  t0=$(date +%s%N)
  for w in $(seq 485 500); do
    ab -q -n $((n / 16)) -c 1 "http://127.0.0.1:$PORT/resize/$w/500/$file" >/dev/null 2>&1 &
    pids+=($!)
  done
  wait "${pids[@]}"
  t1=$(date +%s%N)
  local diverse=$((n * 1000 / ((t1 - t0) / 1000000)))
  # single URL: request coalescing absorbs concurrent duplicates
  local single p50
  read -r single p50 < <(ab -q -n "$n" -c "$C" \
    "http://127.0.0.1:$PORT/resize/500/500/$file" 2>/dev/null |
    awk '/Requests per second/{r=$4} /^  50%/{p=$2} END{print r, p}')
  local peak
  peak=$(awk '/VmHWM/{printf "%dMB", $2/1024}' "/proc/$SRV/status" 2>/dev/null || echo "n/a")
  echo "$label (PRESET=$PRESET): diverse=$diverse rps | single-URL c=$C: $single rps p50=${p50}ms | peak RSS=$peak"
}

run medium test-medium.jpg "$N"
run large test-large.jpg $((N / 5))
