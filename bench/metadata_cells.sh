#!/usr/bin/env bash
# Oriented/profiled competitive cells (oximg vs imgproxy, each at its
# defaults): builds dataset variants with an orientation-6 EXIF tag or
# a real sRGB ICC profile inserted (byte surgery, no recompress), then
# runs interleaved cells over each. See METHODOLOGY.md.
#   usage: [SRGB_ICC=/path/to/sRGB.icc] [ROUNDS=2] [DUR=60s] metadata_cells.sh
set -uo pipefail
cd "${HARNESS_DIR:-$HOME/xfmt-harness}"

python3 - "${SRGB_ICC:-}" <<'PYEOF'
import struct, os, shutil, sys
icc = open(sys.argv[1], "rb").read() if sys.argv[1] else bytes((i*131+7) % 251 for i in range(3000))
def app1(o):
    p = b"Exif\x00\x00II" + struct.pack("<H",42) + struct.pack("<I",8)
    p += struct.pack("<H",1) + struct.pack("<H",0x0112) + struct.pack("<H",3)
    p += struct.pack("<I",1) + struct.pack("<H",o) + b"\x00\x00" + struct.pack("<I",0)
    return p
def app2(data):
    return b"ICC_PROFILE\x00" + bytes([1,1]) + data
for variant, segs in [("o6", [(0xE1, app1(6))]), ("icc", [(0xE2, app2(icc))])]:
    d = f"dataset-{variant}"
    shutil.rmtree(d, ignore_errors=True); os.makedirs(d)
    n = 0
    for f in os.listdir("dataset"):
        src = os.path.join("dataset", f)
        if f.endswith(".jpg"):
            b = open(src,"rb").read()
            payload = b"".join(bytes([0xFF, m]) + struct.pack(">H", len(p)+2) + p for m, p in segs)
            open(os.path.join(d, f),"wb").write(b[:2]+payload+b[2:])
            n += 1
        else:
            shutil.copy(src, os.path.join(d, f))
    print(variant, n, "jpgs tagged")
PYEOF

run_cell() { # $1=dataset-dir $2=label $3=OUT_FORMAT(or empty)
  sed -i "s#\${PWD}/dataset[^:]*:#\${PWD}/$1:#" docker-compose.yml
  docker compose up -d --wait nginx oximg imgproxy >/dev/null 2>&1
  sleep 3
  for round in $(seq 1 "${ROUNDS:-2}"); do
    for tool in oximg imgproxy; do
      local args=(-e FORMAT=jpg -e "TOOL=$tool" -e WIDTH=512 -e HEIGHT=512)
      [ -n "$3" ] && args+=(-e "OUT_FORMAT=$3")
      out=$(docker compose run --rm k6 run -u 2 -d "${DUR:-60s}" "${args[@]}" k6.js 2>&1)
      rate=$(echo "$out" | awk "/http_reqs/{print \$3}")
      med=$(echo "$out" | grep -o "med=[^ ]*" | head -1)
      echo "$(date +%H:%M) $2 r$round $tool: $rate $med"
    done
  done
  docker compose down >/dev/null 2>&1
}
sed -i "s/cpuset: \"[^\"]*\"/cpuset: \"${CPUSET:-0,1}\"/" docker-compose.yml
run_cell dataset      clean-jpg ""
run_cell dataset-o6   o6-jpg    ""
run_cell dataset-icc  icc-jpg   ""
run_cell dataset-o6   o6-avif   avif
run_cell dataset-icc  icc-avif  avif
sed -i "s#\${PWD}/dataset[^:]*:#\${PWD}/dataset:#" docker-compose.yml
sed -i "s/cpuset: \"[^\"]*\"/cpuset: \"0-1\"/" docker-compose.yml
echo "META_CELLS DONE"
