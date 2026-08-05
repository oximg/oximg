#!/usr/bin/env bash
# Connection-churn cell: does oximg's HTTP client REUSE origin
# connections under burst-shaped load, and what does the churn cost?
#
# Mechanism under test (issue #20 follow-up): a remote-source
# deployment fetches with OXIMG_FETCH_CONCURRENCY-way bursts against
# ONE host, so any client whose pool keeps fewer idle connections than
# the burst is wide discards and re-establishes them constantly —
# measured on ureq 3.3 (3 idle per host): 5 of every 8-wide burst
# re-established, srv_fetch +12ms at a modeled 20ms setup. Every
# discarded connection is a TCP+TLS setup the next burst pays again;
# the reqwest migration retired the cost (h2 = one multiplexed
# connection), and this cell is what verifies that claim — and guards
# it against future client changes.
#
# The lab cannot do TLS-over-RTT, so the origin charges SETUP_MS once
# per NEW connection (a stated model: TLS 1.3 + TCP is ~2 round trips
# + crypto, which a reused connection never pays) and counts data
# connections. conns/req is ground truth for how often the cost was
# paid; srv_fetch shows it landing in the phase="fetch" metric.
#
# Interleaves two IMAGES in one window (the pool config is baked into
# the binary, so this is an image A/B, not an env A/B — the one
# methodology deviation from ab.sh, stated here deliberately).
set -euo pipefail

IMAGE_A=${IMAGE_A:-ghcr.io/oximg/oximg:0.9.0}
IMAGE_B=${IMAGE_B:-oximg:poolpoc}
LATENCY_MS=${LATENCY_MS:-30}
SETUP_MS=${SETUP_MS:-20}
GAP_MS=${GAP_MS:-0}
CPUS=${CPUS:-1}
SERVER_CPUSET=${SERVER_CPUSET:-0}
LOAD_CPUSET=${LOAD_CPUSET:-4-11}
BURST=${BURST:-8}
ROUNDS=${ROUNDS:-12}
REPEATS=${REPEATS:-2}
WORKERS=${WORKERS:-1}
FETCH_CONCURRENCY=${FETCH_CONCURRENCY:-8}
FILE=${FILE:-photo.jpg}
WIDTHS=${WIDTHS:-1200,1300,1400,1500,1600,1700,1800,1900}
PORT=${PORT:-18100}
ORIGIN_PORT=${ORIGIN_PORT:-18099}
LAB=$(cd "$(dirname "$0")" && pwd)

cleanup() { docker rm -f churnlab >/dev/null 2>&1 || true; [ -n "${ORIGIN_PID:-}" ] && kill "$ORIGIN_PID" 2>/dev/null || true; }
trap cleanup EXIT

ROOT="$LAB/fixtures" LATENCY_MS="$LATENCY_MS" SETUP_MS="$SETUP_MS" PORT="$ORIGIN_PORT" \
  taskset -c "$LOAD_CPUSET" python3 "$LAB/origin.py" &
ORIGIN_PID=$!
sleep 0.5

counter() { curl -sf "localhost:$ORIGIN_PORT/$1"; }

run() { # image label -> one result line
  local image=$1 label=$2
  docker rm -f churnlab >/dev/null 2>&1 || true
  docker run -d --name churnlab --network host \
    --cpus="$CPUS" --cpuset-cpus="$SERVER_CPUSET" \
    -e PORT="$PORT" -e OXIMG_METRICS=1 -e OXIMG_WORKERS="$WORKERS" \
    -e OXIMG_FETCH_CONCURRENCY="$FETCH_CONCURRENCY" \
    -e OXIMG_SOURCE_BASE_URL="http://127.0.0.1:$ORIGIN_PORT" \
    "$image" >/dev/null
  for _ in $(seq 1 100); do curl -sf "localhost:$PORT/health" >/dev/null && break; sleep 0.2; done
  # Warm to the steady state OF THIS SHAPE: two bursts, then one gap,
  # so the measured window starts with whatever pool the gap leaves.
  taskset -c "$LOAD_CPUSET" python3 "$LAB/burst.py" --base "http://localhost:$PORT" \
    --file "$FILE" --burst "$BURST" --rounds 2 --warmup 0 --widths "$WIDTHS" >/dev/null
  [ "$GAP_MS" != 0 ] && sleep "$(awk "BEGIN{print $GAP_MS/1000}")"
  local c0 n0 c1 n1
  c0=$(counter __conns); n0=$(counter __count)
  printf '%-8s gap=%-6s ' "$label" "${GAP_MS}ms"
  taskset -c "$LOAD_CPUSET" python3 "$LAB/burst.py" --base "http://localhost:$PORT" \
    --file "$FILE" --burst "$BURST" --rounds "$ROUNDS" --warmup 0 \
    --gap-ms "$GAP_MS" --widths "$WIDTHS" | tr -d '\n'
  c1=$(counter __conns); n1=$(counter __count)
  awk "BEGIN{printf \" conns=%d reqs=%d conns/req=%.2f\n\", $c1-$c0, $n1-$n0, ($c1-$c0)/($n1-$n0)}"
  docker rm -f churnlab >/dev/null 2>&1 || true
}

echo "=== churn cell: latency=${LATENCY_MS}ms setup=${SETUP_MS}ms gap=${GAP_MS}ms burst=$BURST fetch_slots=$FETCH_CONCURRENCY rounds=$ROUNDS ==="
for r in $(seq 1 "$REPEATS"); do
  run "$IMAGE_A" A:stock
  run "$IMAGE_B" B:poc
done
