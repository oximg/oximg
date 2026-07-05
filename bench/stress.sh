#!/usr/bin/env bash
# Connection-capacity ramp: how many concurrent connections the server
# sustains and how it degrades — throughput, latency percentiles,
# failures, and peak memory per level. Server and load generator run in
# Docker on disjoint cpusets so they never contend; each concurrency
# level restarts the server for a clean memory.peak.
#
# Usage: DATASET=~/xfmt-harness/dataset bench/stress.sh
#        LEVELS="16 256 4096" TOOLS="oximg" bench/stress.sh
set -uo pipefail
cd "$(dirname "$0")/.."

DATASET=${DATASET:?path to the harness dataset (0801.jpg..0900.jpg)}
LEVELS=${LEVELS:-"16 64 256 1024 2048 4096 8192"}
TOOLS=${TOOLS:-"oximg imgproxy"}
DURATION=${DURATION:-30s}
SERVER_CPUS=${SERVER_CPUS:-"0-3,8-11"}
LOADGEN_CPUS=${LOADGEN_CPUS:-"4-7,12-15"}
OXIMG_IMAGE=${OXIMG_IMAGE:-oximg:bench}
IMGPROXY_IMAGE=${IMGPROXY_IMAGE:-ghcr.io/imgproxy/imgproxy:latest}
PORT=48620

start_server() { # $1=tool
  docker rm -f stress-srv >/dev/null 2>&1 || true
  case $1 in
    oximg)
      docker run -d --name stress-srv --cpuset-cpus="$SERVER_CPUS" \
        --ulimit nofile=65536:65536 -e PORT=80 \
        -p 127.0.0.1:$PORT:80 -v "$DATASET":/images:ro "$OXIMG_IMAGE" ;;
    imgproxy)
      docker run -d --name stress-srv --cpuset-cpus="$SERVER_CPUS" \
        --ulimit nofile=65536:65536 \
        -e IMGPROXY_LOCAL_FILESYSTEM_ROOT=/images \
        -p 127.0.0.1:$PORT:8080 -v "$DATASET":/images:ro "$IMGPROXY_IMAGE" ;;
  esac >/dev/null
  for _ in $(seq 200); do
    curl -sf -o /dev/null --max-time 0.5 "http://127.0.0.1:$PORT/health" && return
    sleep 0.1
  done
  echo "server failed to become ready" >&2
  exit 1
}

echo "=== connection-capacity ramp (server cpus $SERVER_CPUS, loadgen $LOADGEN_CPUS, $DURATION/level) ==="
for tool in $TOOLS; do
  for vus in $LEVELS; do
    start_server "$tool"
    # short warmup so page cache and pools are hot
    docker run --rm --network host --cpuset-cpus="$LOADGEN_CPUS" \
      --ulimit nofile=65536:65536 -v "$PWD/bench":/b:ro grafana/k6 run -q \
      -e BASE="http://127.0.0.1:$PORT" -e KIND="$tool" -e VUS=16 -e DURATION=5s \
      /b/stress.js >/dev/null 2>&1
    out=$(docker run --rm --network host --cpuset-cpus="$LOADGEN_CPUS" \
      --ulimit nofile=65536:65536 -v "$PWD/bench":/b:ro grafana/k6 run -q \
      -e BASE="http://127.0.0.1:$PORT" -e KIND="$tool" -e VUS="$vus" -e DURATION="$DURATION" \
      /b/stress.js 2>&1)
    rps=$(grep -oE "http_reqs[^:]*: +[0-9]+ +[0-9.]+" <<<"$out" | awk '{print $NF}')
    lat=$(grep -m1 -E "http_req_duration\.\." <<<"$out" |
      grep -oE "(p\(50\)|p\(95\)|p\(99\)|max)=[0-9.]+m?s" | tr '\n' ' ')
    fail=$(grep -m1 "http_req_failed" <<<"$out" | grep -oE "[0-9.]+%" | head -1)
    peak=$(docker exec stress-srv cat /sys/fs/cgroup/memory.peak 2>/dev/null |
      awk '{printf "%dMB", $1/1048576}')
    echo "$tool c=$vus: rps=$rps $lat failed=${fail:-0%} peakRSS=$peak"
  done
done
docker rm -f stress-srv >/dev/null 2>&1 || true
