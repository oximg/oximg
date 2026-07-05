#!/usr/bin/env bash
# Cold start: process/container start until the first 200, measured
# twice per run — /health (listener ready) and the first real /resize
# (any lazy pipeline init included). Native binary and Docker, oximg vs
# imgproxy (Docker only; matches the container platforms where cold
# start matters). Distributions, not averages: the tail is the problem.
#
# Usage: bench/coldstart.sh            (needs target/release/oximg,
#        docker images oximg:bench and ghcr.io/imgproxy/imgproxy)
set -uo pipefail
cd "$(dirname "$0")/.."

N_NATIVE=${N_NATIVE:-20}
N_DOCKER=${N_DOCKER:-10}
FIXTURES=${FIXTURES:-$PWD/tests/fixtures}
PORT=${PORT:-48610}
OXIMG_IMAGE=${OXIMG_IMAGE:-oximg:bench}
IMGPROXY_IMAGE=${IMGPROXY_IMAGE:-ghcr.io/imgproxy/imgproxy:latest}

now_ms() { echo $(($(date +%s%N) / 1000000)); }

wait200() { # $1=url $2=t0_ms — echoes elapsed ms (or TIMEOUT)
  local url=$1 t0=$2
  while :; do
    if curl -sf -o /dev/null --max-time 0.5 "$url"; then
      echo $(($(now_ms) - t0))
      return
    fi
    [ $(($(now_ms) - t0)) -gt 30000 ] && { echo TIMEOUT; return; }
  done
}

stats() { # stdin: one ms value per line
  sort -n | awk '{a[NR]=$1} END{
    printf "min=%4d  p50=%4d  p95=%4d  max=%4d ms  (n=%d)\n",
      a[1], a[int(NR*0.5+0.5)], a[int(NR*0.95+0.5)], a[NR], NR}'
}

run_series() { # $1=label $2=n $3=start_cmd $4=stop_cmd $5=health_url $6=work_url
  local label=$1 n=$2 start=$3 stop=$4 health=$5 work=$6
  : >/tmp/cs-health; : >/tmp/cs-work
  for _ in $(seq "$n"); do
    local t0 th tw
    t0=$(now_ms)
    eval "$start" >/dev/null 2>&1
    th=$(wait200 "$health" "$t0")
    tw=$(wait200 "$work" "$t0")
    echo "$th" >>/tmp/cs-health
    echo "$tw" >>/tmp/cs-work
    eval "$stop" >/dev/null 2>&1
  done
  printf "%-28s ready:      " "$label"
  stats </tmp/cs-health
  printf "%-28s first work: " ""
  stats </tmp/cs-work
}

echo "=== cold start (host: $(uname -m), $(nproc) cpus) ==="

run_series "oximg native" "$N_NATIVE" \
  "PORT=$PORT IMAGES_DIR=$FIXTURES ./target/release/oximg & echo \$! > /tmp/cs-pid" \
  "kill \$(cat /tmp/cs-pid); wait \$(cat /tmp/cs-pid) 2>/dev/null" \
  "http://127.0.0.1:$PORT/health" \
  "http://127.0.0.1:$PORT/resize/100/100/photo.jpg"

run_series "oximg docker ($OXIMG_IMAGE)" "$N_DOCKER" \
  "docker run -d --name cs-ox -e PORT=80 -p 127.0.0.1:$PORT:80 -v $FIXTURES:/images:ro $OXIMG_IMAGE" \
  "docker rm -f cs-ox" \
  "http://127.0.0.1:$PORT/health" \
  "http://127.0.0.1:$PORT/resize/100/100/photo.jpg"

run_series "imgproxy docker" "$N_DOCKER" \
  "docker run -d --name cs-im -e IMGPROXY_LOCAL_FILESYSTEM_ROOT=/images -p 127.0.0.1:$PORT:8080 -v $FIXTURES:/images:ro $IMGPROXY_IMAGE" \
  "docker rm -f cs-im" \
  "http://127.0.0.1:$PORT/health" \
  "http://127.0.0.1:$PORT/insecure/rs:fit:100:100/plain/local:///photo.jpg"

echo
echo "image sizes:"
for img in "$OXIMG_IMAGE" "$IMGPROXY_IMAGE"; do
  docker images --format "  {{.Repository}}:{{.Tag}}  {{.Size}}" "$img" 2>/dev/null | head -1
done
echo "binary size:"
ls -lh target/release/oximg | awk '{print "  oximg native  " $5}'
