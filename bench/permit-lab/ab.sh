#!/usr/bin/env bash
# Interleaved A/B over OXIMG_WORKERS at a fixed CPU budget, for one
# origin-latency cell. Same binary, env toggle (methodology rule 3), A
# and B alternating inside one window (rule 1) because ambient load
# drifts on the scale of minutes.
#
# The question: oximg holds a CPU permit across the *whole* request,
# including the origin fetch. If that wastes the quota on network wait,
# a second permit should raise throughput at an unchanged 1-CPU budget,
# and the gain should grow with origin latency. Local-file sources are
# the control: no IO to hide, so a second permit should buy nothing.
set -euo pipefail

IMAGE=${IMAGE:-ghcr.io/oximg/oximg:0.8.1}
SRC=${SRC:-http}                  # http | local
LATENCY_MS=${LATENCY_MS:-0}
CPUS=${CPUS:-1}
SERVER_CPUSET=${SERVER_CPUSET:-0} # exclusive core: the generator must not steal it
LOAD_CPUSET=${LOAD_CPUSET:-4-11}
BURST=${BURST:-8}
ROUNDS=${ROUNDS:-20}
REPEATS=${REPEATS:-3}
FILE=${FILE:-photo.jpg}
# Output widths decide the encode cost, which dominates CPU time. Set
# these near the widths a real deployment serves (production reports
# 1920) rather than leaving the default spread, or the cell measures a
# much cheaper service than the one being reasoned about.
WIDTHS=${WIDTHS:-}
WORKERS_A=${WORKERS_A:-1}
WORKERS_B=${WORKERS_B:-2}
PORT=${PORT:-18100}   # oximg's own listen port (host network)
ORIGIN_PORT=${ORIGIN_PORT:-18099}
LAB=$(cd "$(dirname "$0")" && pwd)

cleanup() { docker rm -f permitlab >/dev/null 2>&1 || true; [ -n "${ORIGIN_PID:-}" ] && kill "$ORIGIN_PID" 2>/dev/null || true; }
trap cleanup EXIT

if [ "$SRC" = http ]; then
  ROOT="$LAB/fixtures" LATENCY_MS="$LATENCY_MS" PORT="$ORIGIN_PORT" \
    taskset -c "$LOAD_CPUSET" python3 "$LAB/origin.py" &
  ORIGIN_PID=$!
  sleep 0.5
fi

run() { # workers -> one result line
  local workers=$1
  docker rm -f permitlab >/dev/null 2>&1 || true
  # Host networking, for both source modes: a bridge-networked
  # container could not reach an origin on the host (docker0 -> host
  # INPUT is firewalled on this box), and using it for the local cell
  # too keeps the two conditions identical — no docker-proxy NAT in one
  # arm and not the other. CPU quota and cpuset are cgroup settings and
  # are unaffected by the network namespace.
  local args=(-d --name permitlab --network host
    --cpus="$CPUS" --cpuset-cpus="$SERVER_CPUSET"
    -e PORT="$PORT" -e OXIMG_METRICS=1 -e OXIMG_WORKERS="$workers")
  if [ "$SRC" = http ]; then
    args+=(-e OXIMG_SOURCE_BASE_URL="http://127.0.0.1:$ORIGIN_PORT")
  else
    args+=(-v "$LAB/fixtures":/images:ro)
  fi
  docker run "${args[@]}" "$IMAGE" >/dev/null
  for _ in $(seq 1 100); do curl -sf "localhost:$PORT/health" >/dev/null && break; sleep 0.2; done
  printf 'W=%-2s src=%-5s lat=%-4s ' "$workers" "$SRC" "$LATENCY_MS"
  taskset -c "$LOAD_CPUSET" python3 "$LAB/burst.py" --base "http://localhost:$PORT" \
    --file "$FILE" --burst "$BURST" --rounds "$ROUNDS" ${WIDTHS:+--widths "$WIDTHS"}
  docker rm -f permitlab >/dev/null 2>&1 || true
}

echo "=== cell: src=$SRC latency=${LATENCY_MS}ms cpus=$CPUS burst=$BURST rounds=$ROUNDS ==="
for r in $(seq 1 "$REPEATS"); do
  run "$WORKERS_A"
  run "$WORKERS_B"
done
