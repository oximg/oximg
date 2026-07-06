#!/usr/bin/env bash
# Regenerate the reproducible fixtures (see README.md). Requires
# Docker; uses alpine's libavif-apps + ImageMagick so the writers are
# independent of oximg's own code.
set -euo pipefail
cd "$(dirname "$0")"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The deterministic ICC blob (mirrors tests/common/mod.rs fake_icc(900))
# and the 240x180 corner image (mirrors corner_base), plus solid
# animation frames.
python3 - "$work" <<'PYEOF'
import struct, sys, zlib

work = sys.argv[1]
open(f"{work}/fake.icc", "wb").write(bytes((i * 131 + 7) % 251 for i in range(900)))

def png(path, w, h, px):
    raw = b"".join(b"\x00" + bytes(px[y * w * 3:(y + 1) * w * 3]) for y in range(h))
    def chunk(typ, data):
        c = typ + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c))
    open(path, "wb").write(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b""))

w, h, block = 240, 180, 60
px = bytearray([128] * (w * h * 3))
def fill(x0, y0, rgb):
    for y in range(y0, y0 + block):
        for x in range(x0, x0 + block):
            px[(y * w + x) * 3:(y * w + x) * 3 + 3] = bytes(rgb)
fill(0, 0, [255, 0, 0]); fill(w - block, 0, [0, 255, 0])
fill(0, h - block, [0, 0, 255]); fill(w - block, h - block, [255, 255, 255])
png(f"{work}/corner.png", w, h, px)

for name, rgb in [("f1", [255, 0, 0]), ("f2", [0, 0, 255]), ("f3", [0, 255, 0])]:
    png(f"{work}/{name}.png", 120, 90, bytearray(rgb * (120 * 90)))
PYEOF

docker run --rm -v "$work":/work alpine:3.20 sh -c '
  apk add -q libavif-apps >/dev/null 2>&1
  cd /work
  avifenc -s 10 --icc fake.icc corner.png icc.avif >/dev/null
  avifenc -s 10 --irot 1 corner.png orient_irot1.avif >/dev/null
  avifenc -s 10 --imir 0 corner.png orient_imir0.avif >/dev/null
  avifenc -s 10 --imir 1 corner.png orient_imir1.avif >/dev/null
  avifenc -s 10 --irot 1 --imir 1 corner.png orient_irot1_imir1.avif >/dev/null
  avifenc -s 10 --irot 3 --icc fake.icc corner.png orient_irot3_icc.avif >/dev/null
  avifenc -s 10 --fps 2 f1.png f2.png f3.png anim.avif >/dev/null
  avifenc -s 10 --fps 2 --icc fake.icc --irot 1 f1.png f2.png f3.png anim_meta.avif >/dev/null
'

cp "$work"/icc.avif "$work"/orient_*.avif "$work"/anim.avif "$work"/anim_meta.avif .
echo "regenerated: icc.avif orient_*.avif anim.avif anim_meta.avif"
echo "note: icc.avif was originally encoded from a decoded oximg output"
echo "frame rather than corner.png; only its ICC payload is pinned."
