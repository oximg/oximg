#!/usr/bin/env bash
# Sources shaped like the production corpus in issue #20: a
# photographic JPEG and a large RGB PNG (the format with no
# shrink-on-load, so its cost tracks the source).
set -euo pipefail
LAB=$(cd "$(dirname "$0")" && pwd)
IMAGE=${IMAGE:-ghcr.io/oximg/oximg:0.8.1}
mkdir -p "$LAB/fixtures"
python3 - "$LAB/fixtures" << 'PY'
import struct, sys, zlib
out = sys.argv[1]
def png(path, w, h):
    rows = b''.join(
        b'\x00' + bytes([(x * 7 + y * 3) % 256, (y * 5) % 256, (x * 11) % 256] * 1)
        for y in range(h) for x in [0] if True
    )
    # build rows properly: one filter byte + w RGB pixels per row
    rows = b''
    for y in range(h):
        row = bytearray([0])
        for x in range(w):
            row += bytes([(x * 7 + y * 3) % 256, (y * 5) % 256, (x * 11) % 256])
        rows += bytes(row)
    def chunk(t, d):
        c = struct.pack('>I', len(d)) + t + d
        return c + struct.pack('>I', zlib.crc32(t + d))
    data = (b'\x89PNG\r\n\x1a\n'
            + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(rows, 6))
            + chunk(b'IEND', b''))
    open(path, 'wb').write(data)
    print(f"{path}: {w}x{h} {len(data)/1e6:.1f}MB")
png(f"{out}/src.png", 2000, 1500)
png(f"{out}/big.png", 2250, 2600)
PY
# Transcode one source to JPEG through oximg itself, so the JPEG cell
# exercises the shrink-on-load path with a realistic file.
# --user: the image runs unprivileged, so it cannot write into a
# bind mount owned by the invoking user without being told who to be.
docker run --rm --user "$(id -u):$(id -g)" -v "$LAB/fixtures":/w -w /w "$IMAGE" \
  oximg resize src.png 0 0 photo.jpg -q 82 2>&1 | tail -1
ls -la "$LAB/fixtures" | awk 'NR>1 {printf "%-12s %s\n", $9, $5}'
