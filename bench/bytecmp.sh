#!/usr/bin/env bash
# Byte-identity compare between two oximg images over the benchmark
# dataset: every URL must hash identically.
#   usage: bytecmp.sh IMAGE_A IMAGE_B [DATASET_DIR]
set -uo pipefail
A=${1:?image A}; B=${2:?image B}
DATA=${3:-${HARNESS_DIR:-$HOME/xfmt-harness}/dataset}
PA=${PORT_A:-9101}; PB=${PORT_B:-9102}
run() {
  docker run -d --rm --name "bc_$2" --cpuset-cpus "${CPUSET:-0-1}" \
    -v "$DATA":/images:ro -e IMAGES_DIR=/images -e PORT=80 \
    -p "$2":80 "$1" >/dev/null
}
docker rm -f "bc_$PA" "bc_$PB" >/dev/null 2>&1
run "$A" "$PA"; run "$B" "$PB"
sleep 2
urls=()
for n in 0801 0802 0803; do
  for ext in jpg png webp avif; do urls+=("/resize/512/512/$n.$ext"); done
done
urls+=(/resize/512/512/0801.jpg@webp /resize/512/512/0801.jpg@avif
  /resize/512/512/0801.jpg@png /resize/512/512/0801.png@jpg
  /resize/512/512/0801.webp@jpg /resize/512/512/0801.avif@jpg)
pass=0 fail=0
for u in "${urls[@]}"; do
  ha=$(curl -sf "http://127.0.0.1:$PA$u" | sha256sum | cut -d' ' -f1)
  hb=$(curl -sf "http://127.0.0.1:$PB$u" | sha256sum | cut -d' ' -f1)
  if [ -n "$ha" ] && [ "$ha" = "$hb" ]; then pass=$((pass+1)); else
    fail=$((fail+1)); echo "DIFF $u a=$ha b=$hb"; fi
done
echo "byte-compare: $pass/$((pass+fail)) identical"
docker rm -f "bc_$PA" "bc_$PB" >/dev/null 2>&1
[ "$fail" = 0 ]
