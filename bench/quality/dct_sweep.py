#!/usr/bin/env python3
"""Quality as a function of the JPEG DCT decode scale.

`OXIMG_DCT_MARGIN` decides how much of a downscale is handed to
libjpeg's scaled decode and how much is left to the resampler. The
old default (1.7) selected the 3/8 scale for a 5.3x downscale, which
measured 13.4 SSIMULACRA2 points below a full decode — for the same
output size and the same bytes. ImageMagick reproduces the same dip
through its own `jpeg:size` hint, so the effect is libjpeg's reduced
IDCT and not this pipeline's.

Three photographs at one ratio were not enough to move a default that
shapes every user's output, which is what this exists for: every
reachable numerator, over the whole quality corpus (24 Kodak plus 12
photographs per size group), at several ratios.

Each cell: oximg decodes at k/8, resizes to the target, encodes at q80.
Scored with SSIMULACRA2 against a linear-light Lanczos downscale at the
same dimensions — QUALITY.md's ref_lin, built the same way.

  python3 bench/quality/dct_sweep.py [--quality 80] [--bin target/release/oximg]
"""

import argparse
import json
import math
import os
import pathlib
import subprocess
import statistics
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
CORPUS = ROOT / "bench/quality/corpus"

# (label, glob, [target widths]). The targets span the ratios real
# traffic asks for: a thumbnail, a card, and a content-width image.
GROUPS = [
    ("kodak 768x512", "src/*.jpg", [500, 400]),
    ("medium 2000x1334", "medium/*.jpg", [750, 500]),
    ("large 4000x2667", "large/*.jpg", [750, 500]),
]


def sweep_env(margin):
    """The caller's environment, plus this sweep's margin.

    Inherited rather than replaced: a stripped-down env breaks anything
    that needs a dynamic-linker path, a locale, or a toolchain prefix,
    and this harness is meant to run on machines it was not written on.
    Every *other* OXIMG_* knob is dropped, though — a shell that
    happens to export OXIMG_RESIZE=srgb would otherwise change what the
    sweep measures without saying so.
    """
    env = {k: v for k, v in os.environ.items() if not k.startswith("OXIMG_")}
    env["OXIMG_DCT_MARGIN"] = f"{margin:.6f}"
    return env


def sh(*cmd, env=None):
    r = subprocess.run([str(c) for c in cmd], capture_output=True, text=True, env=env)
    if r.returncode != 0:
        raise RuntimeError(f"{cmd[0]} failed: {r.stderr.strip()}")
    return r.stdout


def dims(path):
    return tuple(int(v) for v in sh("magick", "identify", "-format", "%w %h", path).split())


def margin_for(src_w, dst_w, k):
    """The OXIMG_DCT_MARGIN that makes dct_scale_num pick k.

    The knob is `need = ceil(dst * margin)`, and k is the smallest
    numerator whose decoded width reaches `need`. Asking for exactly
    the decoded width of k lands on k. Unreachable when the decode
    would fall below the target (the pipeline never decodes smaller
    than it needs) or when the margin exceeds the knob's own 8.0 cap.
    """
    decoded = math.ceil(src_w * k / 8)
    if decoded < dst_w:
        return None
    m = decoded / dst_w
    return m if 1.0 <= m <= 8.0 else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--quality", type=int, default=80)
    ap.add_argument("--bin", default=str(ROOT / "target/release/oximg"))
    args = ap.parse_args()

    if not pathlib.Path(args.bin).is_file():
        sys.exit(f"{args.bin} not found — cargo build --release")

    rows = []
    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp)
        for label, glob, targets in GROUPS:
            sources = sorted(CORPUS.glob(glob))
            if not sources:
                print(f"skip {label}: no sources", file=sys.stderr)
                continue
            src_w = dims(sources[0])[0]
            for target in targets:
                per_k = {}
                for k in range(1, 9):
                    m = margin_for(src_w, target, k)
                    if m is None:
                        continue
                    scores, sizes = [], []
                    for src in sources:
                        out = tmp / f"{label[:3]}-{target}-{k}-{src.name}"
                        sh(args.bin, "resize", src, target, 0, out,
                           "-q", args.quality, env=sweep_env(m))
                        w, h = dims(out)
                        ref = tmp / f"ref-{src.stem}-{w}x{h}.png"
                        if not ref.exists():
                            sh("magick", src, "-colorspace", "RGB", "-filter", "Lanczos",
                               "-resize", f"{w}x{h}!", "-colorspace", "sRGB", ref)
                        scores.append(float(sh("ssimulacra2", ref, out).strip()))
                        sizes.append(out.stat().st_size)
                    per_k[k] = {
                        "margin": round(m, 4),
                        "decoded_px": math.ceil(src_w * k / 8),
                        "ssim2": statistics.mean(scores),
                        "kb": statistics.mean(sizes) / 1024,
                    }
                    print(".", end="", flush=True, file=sys.stderr)
                rows.append({"group": label, "target": target, "src_w": src_w,
                             "n": len(sources), "by_k": per_k})
    print(file=sys.stderr)

    # What the current default picks, against the best reachable cell.
    print(f"\nSSIMULACRA2 by DCT numerator, q{args.quality}, "
          "linear-light Lanczos reference at the output's own size\n")
    for row in rows:
        ks = sorted(row["by_k"])
        print(f"## {row['group']} -> {row['target']}px wide  (n={row['n']})")
        print("  k   decode      margin      KB   ssim2")
        best = max(row["by_k"].values(), key=lambda c: c["ssim2"])
        for k in ks:
            c = row["by_k"][k]
            mark = " <- best" if c is best else ""
            print(f"  {k}   {c['decoded_px']:>6}px  {c['margin']:>7.2f}  "
                  f"{c['kb']:>6.1f}  {c['ssim2']:>6.2f}{mark}")
        # The knob picks the smallest k reaching its margin.
        for default, name in ((1.7, "1.7 (the old default)"), (3.0, "3.0")):
            picked = next((k for k in ks if row["by_k"][k]["margin"] >= default), ks[-1])
            c = row["by_k"][picked]
            print(f"  margin {name}: k={picked}, ssim2 {c['ssim2']:.2f} "
                  f"({c['ssim2'] - best['ssim2']:+.2f} vs best)")
        print()

    (ROOT / "bench/quality/dct-sweep.json").write_text(json.dumps(rows, indent=2))


if __name__ == "__main__":
    main()
