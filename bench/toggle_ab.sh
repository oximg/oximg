#!/usr/bin/env bash
# Same-binary env-toggle discriminator (methodology rule 3): separates
# a feature's real per-request cost from code-layout/ambient noise.
#   usage: TOGGLE=OXIMG_ICC [FORMAT=jpg] [ROUNDS=3] toggle_ab.sh IMAGE
set -uo pipefail
IMG=${1:?image}; TOGGLE=${TOGGLE:?toggle env name}
cd "${HARNESS_DIR:-$HOME/xfmt-harness}"
run_one() { # $1=label $2=on|off
  docker tag "$IMG" oximg:bench-run
  sed -i "s/image: oximg:bench.*/image: oximg:bench-run/" docker-compose.yml
  sed -i "s/cpuset: \"[^\"]*\"/cpuset: \"${CPUSET:-0,1}\"/" docker-compose.yml
  sed -i "/$TOGGLE/d" docker-compose.yml
  if [ "$2" = off ]; then
    sed -i "s/      PORT: \"80\"/      PORT: \"80\"\n      $TOGGLE: \"0\"/" docker-compose.yml
  fi
  docker compose up -d --wait nginx oximg >/dev/null 2>&1
  sleep 2
  r=$(docker compose run --rm k6 run -u "${VUS:-2}" -d "${DUR:-120s}" \
    -e "FORMAT=${FORMAT:-jpg}" -e TOOL=oximg -e "WIDTH=${WIDTH:-512}" -e "HEIGHT=${HEIGHT:-512}" k6.js 2>&1 |
    awk "/http_reqs/{print \$3}")
  echo "$(date +%H:%M) $1: $r"
  docker compose down >/dev/null 2>&1
}
for round in $(seq 1 "${ROUNDS:-3}"); do
  run_one "on-r$round" on
  run_one "off-r$round" off
done
sed -i "/$TOGGLE/d" docker-compose.yml
sed -i "s/image: oximg:bench-run/image: oximg:bench/" docker-compose.yml
echo "TOGGLE DONE"
