#!/usr/bin/env bash
# The experiment: does a second permit buy throughput at an unchanged
# 1-CPU budget, and does the gain track origin latency? Local sources
# are the control (no IO inside the permit).
set -uo pipefail
cd "$(dirname "$0")"
export BURST=8 ROUNDS=12 REPEATS=2 FILE=photo.jpg
SRC=local bash ab.sh 2>&1 | grep -E "^===|^W="
for lat in 0 10 30 60; do
  SRC=http LATENCY_MS=$lat bash ab.sh 2>&1 | grep -E "^===|^W=|rps="
done
pkill -f origin.py 2>/dev/null; docker rm -f permitlab >/dev/null 2>&1; true
