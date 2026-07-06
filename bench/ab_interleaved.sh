#!/usr/bin/env bash
# Interleaved same-window A/B on one harness cell (methodology rule 1).
#   usage: FORMAT=jpg [OUT_FORMAT=avif] [ROUNDS=3] [DUR=120s] \
#          [CPUSET=0,1] ab_interleaved.sh IMAGE_A IMAGE_B
set -uo pipefail
A=${1:?image A}; B=${2:?image B}
cd "${HARNESS_DIR:-$HOME/xfmt-harness}"
run_one() {
  docker tag "$1" oximg:bench-run
  sed -i "s/image: oximg:bench.*/image: oximg:bench-run/" docker-compose.yml
  sed -i "s/cpuset: \"[^\"]*\"/cpuset: \"${CPUSET:-0,1}\"/" docker-compose.yml
  docker compose up -d --wait nginx oximg >/dev/null 2>&1
  sleep 2
  local args=(-e "FORMAT=${FORMAT:-jpg}" -e TOOL=oximg -e "WIDTH=${WIDTH:-512}" -e "HEIGHT=${HEIGHT:-512}")
  [ -n "${OUT_FORMAT:-}" ] && args+=(-e "OUT_FORMAT=$OUT_FORMAT")
  r=$(docker compose run --rm k6 run -u "${VUS:-2}" -d "${DUR:-120s}" "${args[@]}" k6.js 2>&1 |
    awk "/http_reqs/{print \$3}")
  echo "$(date +%H:%M) $2: $r"
  docker compose down >/dev/null 2>&1
}
for round in $(seq 1 "${ROUNDS:-3}"); do
  run_one "$A" "A-r$round"
  run_one "$B" "B-r$round"
done
sed -i "s/image: oximg:bench-run/image: oximg:bench/" docker-compose.yml
echo "AB DONE"
