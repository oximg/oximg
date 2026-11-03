#!/usr/bin/env bash
# Peak-memory A/B and CLI byte-identity between two oximg binaries
# (issue #129). No Docker: drives `oximg resize` directly, so it runs on
# macOS hosts where bench/bytecmp.sh cannot.
#   usage: peak-rss.sh BIN_A BIN_B [ROUNDS]
# Prints one JSON object: per case, the median max RSS (MiB) of A and B
# over ROUNDS interleaved runs (A, B, A, B, ...), and whether the two
# outputs hash identically. A case where any run failed reports
# "failed":true, identical false, and null RSS for the failing side.
# RSS is buffer-bound and insensitive to load;
# wall time is not, which is why this reports none.
set -euo pipefail
A=${1:?binary A}; B=${2:?binary B}; ROUNDS=${3:-3}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
FX=${PEAK_RSS_FIXTURES:-$ROOT/target/peak-rss}
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT
mkdir -p "$FX"

# The issue's repro inputs, generated once and kept under target/.
# -seed must come before plasma:fractal, or the image is random. The
# stamp names the generator: inputs cached by an older one (unseeded,
# so different on every host) are regenerated.
STAMP=$FX/.inputs-seeded-v1
if [ ! -f "$STAMP" ]; then
  rm -f "$FX"/huge_8000.jpg "$FX"/raw_7000.png "$FX"/photo_7000.jpg "$FX"/photo_7000*.png
fi
if [ ! -f "$FX/huge_8000.jpg" ]; then
  magick -seed 7 -size 8000x8000 plasma:fractal "$FX/a.png"
  magick "$FX/a.png" -quality 85 "$FX/huge_8000.jpg"
  rm "$FX/a.png"
fi
if [ ! -f "$FX/photo_7000.png" ]; then
  magick -seed 3 -size 7000x7000 plasma:fractal "$FX/raw_7000.png"
  magick "$FX/raw_7000.png" -quality 85 "$FX/photo_7000.jpg"
  magick "$FX/photo_7000.jpg" "$FX/photo_7000.png"
fi
touch "$STAMP"
# Issue #153: the same photo in the other PNG layouts, derived from the
# seeded photo above. Alpha is a flat 50%, as in the issue's repro.
layout() {
  [ -f "$FX/$1" ] || magick "$FX/photo_7000.png" "${@:2}" "$FX/$1"
}
layout photo_7000_rgba.png -alpha set -channel A -evaluate set 50% +channel \
  -define png:color-type=6
layout photo_7000_rgba16.png -alpha set -channel A -evaluate set 50% +channel \
  -depth 16 -define png:color-type=6
layout photo_7000_gray.png -colorspace Gray -define png:color-type=0
layout photo_7000_graya.png -colorspace Gray -alpha set -channel A -evaluate set 50% \
  +channel -define png:color-type=4

# A failed run must return non-zero: time still writes its rusage then,
# and GNU time adds a "Command exited with non-zero status" line first.
case "$(uname)" in
  Darwin) rss() { /usr/bin/time -l -o "$1" "${@:2}" >/dev/null 2>&1 || return 1; awk '/maximum resident/{printf "%.0f", $1/1048576}' "$1"; } ;;
  *) rss() { /usr/bin/time -f %M -o "$1" "${@:2}" >/dev/null 2>&1 || return 1; tail -n1 "$1" | awk '{printf "%.0f", $1/1024}'; } ;;
esac
# Linux hosts may lack shasum; a missing hasher must stop the run, not
# read as "outputs differ".
if command -v sha256sum >/dev/null; then hash() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null; then hash() { shasum -a 256 "$1" | cut -d' ' -f1; }
else echo "peak-rss.sh: need sha256sum or shasum" >&2; exit 1; fi
median() { sort -n | awk '{a[NR]=$1} END{if (NR%2) print a[(NR+1)/2]; else printf "%.0f\n", (a[NR/2]+a[NR/2+1])/2}'; }

# name|env|input|w|h|ext|extra args
CASES=(
  "jpg_full||huge_8000.jpg|0|0|jpg|"
  "jpg_full_seq|OXIMG_JPEG_PROGRESSIVE=0|huge_8000.jpg|0|0|jpg|"
  "jpg_full_fast||huge_8000.jpg|0|0|jpg|--preset fast"
  "jpg_2000||huge_8000.jpg|2000|2000|jpg|"
  "png_thumb||photo_7000.png|10|200|png|"
  "png_thumb_raw||raw_7000.png|10|200|png|"
  "png_thumb_rgba||photo_7000_rgba.png|10|200|png|"
  "png_thumb_rgba16||photo_7000_rgba16.png|10|200|png|"
  "png_thumb_gray||photo_7000_gray.png|10|200|png|"
  "png_thumb_graya||photo_7000_graya.png|10|200|png|"
  "png_thumb_rgba_jpg||photo_7000_rgba.png|10|200|jpg|"
  "png_full||photo_7000.png|0|0|png|"
  "png_to_jpg_full||photo_7000.png|0|0|jpg|"
  "png_to_webp_full||photo_7000.png|0|0|webp|"
)
# Small committed fixtures: byte identity across formats and paths.
for f in "$ROOT"/tests/fixtures/*.{jpg,png,webp}; do
  [ -f "$f" ] || continue
  n=$(basename "$f")
  case "$n" in bogus_*) continue ;; esac
  CASES+=("fx_${n}_full||$f|0|0|${n##*.}|" "fx_${n}_64||$f|64|64|${n##*.}|")
done

printf '{"rounds":%s,"cases":[' "$ROUNDS"
sep=
for c in "${CASES[@]}"; do
  IFS='|' read -r name envs input w h ext extra <<<"$c"
  [ "${input:0:1}" = / ] || input=$FX/$input
  ra=() rb=() failed= fa= fb=
  for _ in $(seq "$ROUNDS"); do
    for side in a b; do
      bin=$A; [ $side = b ] && bin=$B
      # A stale output from an earlier round must not be hashed.
      rm -f "$OUT/$side.$ext"
      # shellcheck disable=SC2086
      if ! r=$(rss "$OUT/t" env $envs "$bin" resize "$input" "$w" "$h" "$OUT/$side.$ext" $extra); then
        r=-1 failed=1
        if [ $side = a ]; then fa=1; else fb=1; fi
      fi
      if [ $side = a ]; then ra+=("$r"); else rb+=("$r"); fi
    done
  done
  ha=$(hash "$OUT/a.$ext" 2>/dev/null || echo none)
  hb=$(hash "$OUT/b.$ext" 2>/dev/null || echo none)
  # Any failed round voids the identity verdict for the case.
  [ -n "$failed" ] && ha=failed-a hb=failed-b
  same=false; [ "$ha" = "$hb" ] && [ "$ha" != none ] && same=true
  # A side with any failed round has no RSS figure: a median over the
  # surviving rounds would read as a clean measurement.
  rssa=null; [ -z "$fa" ] && rssa=$(printf '%s\n' "${ra[@]}" | median)
  rssb=null; [ -z "$fb" ] && rssb=$(printf '%s\n' "${rb[@]}" | median)
  failj=false; [ -n "$failed" ] && failj=true
  printf '%s{"case":"%s","rss_a_mib":%s,"rss_b_mib":%s,"identical":%s,"failed":%s}' "$sep" "$name" \
    "$rssa" "$rssb" "$same" "$failj"
  sep=,
  rm -f "$OUT"/a.* "$OUT"/b.*
done
printf ']}\n'
