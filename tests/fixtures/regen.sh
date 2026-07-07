#!/usr/bin/env bash
# Regenerate the reproducible fixtures (see README.md). Requires
# Docker; uses alpine's libavif-apps + ImageMagick + libvips +
# libjpeg-turbo so the writers are independent of oximg's own code.
set -euo pipefail
cd "$(dirname "$0")"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The deterministic ICC blob (mirrors tests/common/mod.rs fake_icc(900)),
# the 240x180 corner image (mirrors corner_base), solid animation
# frames, the 64x48 corner image the CMYK family uses, and the python
# surgeries the CMYK recipes need.
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

def corner(path, w, h, block):
    px = bytearray([128] * (w * h * 3))
    def fill(x0, y0, rgb):
        for y in range(y0, y0 + block):
            for x in range(x0, x0 + block):
                px[(y * w + x) * 3:(y * w + x) * 3 + 3] = bytes(rgb)
    fill(0, 0, [255, 0, 0]); fill(w - block, 0, [0, 255, 0])
    fill(0, h - block, [0, 0, 255]); fill(w - block, h - block, [255, 255, 255])
    png(path, w, h, px)

corner(f"{work}/corner.png", 240, 180, 60)
corner(f"{work}/corner64.png", 64, 48, 16)

for name, rgb in [("f1", [255, 0, 0]), ("f2", [0, 0, 255]), ("f3", [0, 255, 0])]:
    png(f"{work}/{name}.png", 120, 90, bytearray(rgb * (120 * 90)))

# --- CMYK helper scripts, run inside the container ---

# Drop the Adobe APP14 (0xEE) segment: jpegtran -copy none cannot do
# this (libjpeg re-emits APP14 for CMYK).
open(f"{work}/strip_app14.py", "w").write('''
import struct, sys
d = open(sys.argv[1], "rb").read()
out = bytearray(d[:2]); i = 2
while i + 4 <= len(d) and d[i] == 0xFF:
    m = d[i + 1]
    if 0xD0 <= m <= 0xD9 or m == 0x01:
        out += d[i:i + 2]; i += 2; continue
    ln = struct.unpack(">H", d[i + 2:i + 4])[0]
    if m == 0xDA:
        out += d[i:]; break
    if m != 0xEE:
        out += d[i:i + 2 + ln]
    i += 2 + ln
open(sys.argv[2], "wb").write(bytes(out))
''')

# Reassemble the APP2 ICC chain out of a JPEG (chunks sorted by seq).
open(f"{work}/icc_extract.py", "w").write('''
import struct, sys
d = open(sys.argv[1], "rb").read()
i = 2; chunks = []
while i + 4 <= len(d) and d[i] == 0xFF:
    m = d[i + 1]
    if 0xD0 <= m <= 0xD9 or m == 0x01:
        i += 2; continue
    ln = struct.unpack(">H", d[i + 2:i + 4])[0]
    body = d[i + 4:i + 2 + ln]
    if m == 0xE2 and body[:12] == b"ICC_PROFILE\\x00":
        chunks.append((body[12], body[14:]))
    if m == 0xDA: break
    i += 2 + ln
chunks.sort()
open(sys.argv[2], "wb").write(b"".join(c for _, c in chunks))
''')

# Strip a LUT CMYK profile to its color-transform essentials: keep
# desc/cprt/wtpt/bkpt/A2B0/B2A0, alias the other per-intent tags to
# the same data blocks (legal per ICC spec; without them lcms warns
# and falls back to perceptual), zero the profile-ID. Argyll
# "Chemical proof" goes 961KB -> ~108KB (the bulk is targ/DevD/CIED
# characterization text).
open(f"{work}/icc_strip.py", "w").write('''
import struct, sys
src = open(sys.argv[1], "rb").read()
count = struct.unpack(">I", src[128:132])[0]
tags = {}
for i in range(count):
    off = 132 + i * 12
    sig = src[off:off + 4]
    o, sz = struct.unpack(">II", src[off + 4:off + 12])
    tags[sig] = src[o:o + sz]
keep = [b"desc", b"cprt", b"wtpt", b"bkpt", b"A2B0", b"B2A0"]
alias = {b"A2B1": b"A2B0", b"A2B2": b"A2B0", b"B2A1": b"B2A0", b"B2A2": b"B2A0"}
order = keep + sorted(alias)
pad4 = lambda x: (x + 3) // 4 * 4
pos = 132 + len(order) * 12
offsets = {}
for sig in keep:
    offsets[sig] = pos
    pos += pad4(len(tags[sig]))
table = struct.pack(">I", len(order))
for sig in order:
    t = alias.get(sig, sig)
    table += sig + struct.pack(">II", offsets[t], len(tags[t]))
body = b"".join(tags[s] + b"\\x00" * (pad4(len(tags[s])) - len(tags[s])) for s in keep)
hdr = bytearray(src[:128])
hdr[0:4] = struct.pack(">I", 132 + len(order) * 12 + len(body))
hdr[84:100] = bytes(16)  # profile ID: zero per spec for unsigned
open(sys.argv[2], "wb").write(bytes(hdr) + table + body)
''')

# Splice APP2 ICC chunks into a JPEG right after its APP14 segment
# (magick cannot embed a profile without also being given a source
# profile file, and vips re-encodes to transform=0 -- the splice
# keeps the YCCK pixels byte-identical).
open(f"{work}/icc_embed.py", "w").write('''
import struct, sys
d = open(sys.argv[1], "rb").read()
icc = open(sys.argv[2], "rb").read()
CHUNK = 65519 - 14
parts = [icc[i:i + CHUNK] for i in range(0, len(icc), CHUNK)]
app2 = b""
for n, part in enumerate(parts):
    body = b"ICC_PROFILE\\x00" + bytes([n + 1, len(parts)]) + part
    app2 += b"\\xff\\xe2" + struct.pack(">H", len(body) + 2) + body
i = 2  # find the insertion point: after APP0/APP14, before SOF/DQT
while i + 4 <= len(d) and d[i] == 0xFF and d[i + 1] in (0xE0, 0xEE):
    i += 2 + struct.unpack(">H", d[i + 2:i + 4])[0]
open(sys.argv[3], "wb").write(d[:i] + app2 + d[i:])
''')
PYEOF

docker run --rm -v "$work":/work alpine:3.20 sh -c '
  apk add -q libavif-apps imagemagick libjpeg-turbo-utils vips-tools python3 >/dev/null 2>&1
  cd /work
  avifenc -s 10 --icc fake.icc corner.png icc.avif >/dev/null
  avifenc -s 10 --irot 1 corner.png orient_irot1.avif >/dev/null
  avifenc -s 10 --imir 0 corner.png orient_imir0.avif >/dev/null
  avifenc -s 10 --imir 1 corner.png orient_imir1.avif >/dev/null
  avifenc -s 10 --irot 1 --imir 1 corner.png orient_irot1_imir1.avif >/dev/null
  avifenc -s 10 --irot 3 --icc fake.icc corner.png orient_irot3_icc.avif >/dev/null
  avifenc -s 10 --fps 2 f1.png f2.png f3.png anim.avif >/dev/null
  avifenc -s 10 --fps 2 --icc fake.icc --irot 1 f1.png f2.png f3.png anim_meta.avif >/dev/null

  # --- CMYK family (see README.md "CMYK/YCCK fixtures") ---
  magick corner64.png -colorspace CMYK -quality 95 cmyk_ycck.jpg
  vips copy cmyk_ycck.jpg "cmyk_t0.jpg[Q=95,strip]"
  jpegtran -progressive cmyk_ycck.jpg > cmyk_prog.jpg
  magick corner64.png -colorspace CMYK -quality 85 \
    -sampling-factor 2x2,1x1,1x1,2x2 cmyk_sub.jpg
  python3 strip_app14.py cmyk_t0.jpg cmyk_noadobe.jpg

  # The CMYK ICC fixture: libvips built-in fallback profile (Argyll
  # "Chemical proof", public domain), tag-stripped, spliced into the
  # YCCK baseline.
  vips copy cmyk_ycck.jpg "withprof.jpg[profile=cmyk,Q=95]"
  python3 icc_extract.py withprof.jpg cmyk_full.icc
  python3 icc_strip.py cmyk_full.icc cmyk_strip.icc
  python3 icc_embed.py cmyk_ycck.jpg cmyk_strip.icc cmyk_icc.jpg

  # References: djpeg naive renderings (progressive and no-Adobe
  # variants are exact decode twins of their baselines -- verified
  # here so the shared .ppm stays honest), plus the lcms rendering
  # of the embedded profile via vips.
  djpeg cmyk_ycck.jpg > cmyk_ycck.ppm
  djpeg cmyk_t0.jpg > cmyk_t0.ppm
  djpeg cmyk_sub.jpg > cmyk_sub.ppm
  djpeg cmyk_prog.jpg | cmp - cmyk_ycck.ppm
  djpeg cmyk_noadobe.jpg | cmp - cmyk_t0.ppm
  vips icc_transform cmyk_icc.jpg gt_icc.png srgb --embedded
  # vips ppmsave stamps a timestamp comment (non-reproducible, and
  # strict P6 readers reject comments) -- write the PPM with magick.
  magick gt_icc.png ppm:cmyk_icc.ppm
'

cp "$work"/icc.avif "$work"/orient_*.avif "$work"/anim.avif "$work"/anim_meta.avif .
cp "$work"/cmyk_ycck.jpg "$work"/cmyk_t0.jpg "$work"/cmyk_prog.jpg \
   "$work"/cmyk_noadobe.jpg "$work"/cmyk_sub.jpg "$work"/cmyk_icc.jpg \
   "$work"/cmyk_ycck.ppm "$work"/cmyk_t0.ppm "$work"/cmyk_sub.ppm \
   "$work"/cmyk_icc.ppm .
echo "regenerated: icc.avif orient_*.avif anim.avif anim_meta.avif cmyk_*.jpg cmyk_*.ppm"
echo "note: icc.avif was originally encoded from a decoded oximg output"
echo "frame rather than corner.png; only its ICC payload is pinned."
