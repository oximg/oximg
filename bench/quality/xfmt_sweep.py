#!/usr/bin/env python3
"""Cross-format operating-point sweep: JPEG source -> WebP / AVIF output.

Sweeps OXIMG_WEBP_QUALITY and OXIMG_AVIF_QUALITY over the Kodak corpus
(bench/quality/corpus/src), scoring SSIMULACRA2 against the same
linear-light Lanczos ground truth run.py uses, with oximg's own
JPEG-out q80 default as the anchor row. Candidate outputs are decoded
back to PNG through oximg itself (qcli transcode at unchanged
dimensions is a lossless passthrough), so no external WebP/AVIF decode
delegate is needed.

Usage: xfmt_sweep.py <workdir>   (qcli built with --features avif)
Emits <workdir>/xfmt_results.csv and a per-quality summary on stdout.
"""

import csv
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from statistics import mean

REPO = Path(__file__).resolve().parents[2]
QCLI = REPO / "target/release/examples/qcli"
CORPUS = Path(__file__).resolve().parent / "corpus"
MAXW = MAXH = 500
WEBP_QUALITIES = [60, 70, 75, 80, 85]
AVIF_QUALITIES = [45, 50, 55, 60, 65]

WORK = Path(sys.argv[1]).resolve()
WORK.mkdir(parents=True, exist_ok=True)


def sh(*cmd, env=None):
    e = dict(os.environ, **(env or {}))
    r = subprocess.run([str(c) for c in cmd], capture_output=True, text=True, env=e)
    if r.returncode != 0:
        raise RuntimeError(f"{cmd}: {r.stderr.strip()[:300]}")
    return r


def prepare(src):
    tag = src.stem
    d = WORK / tag
    d.mkdir(exist_ok=True)
    master_ppm = d / "master.ppm"
    r = sh(QCLI, "resize", src, MAXW, MAXH, 80, "fast", d / "_master.jpg", master_ppm)
    w, h = map(int, r.stderr.strip().split("x"))
    ref_lin = d / "ref_linear.png"
    sh("magick", src, "-colorspace", "RGB", "-filter", "Lanczos",
       "-resize", f"{w}x{h}!", "-colorspace", "sRGB", ref_lin)
    return dict(tag=tag, src=src, dir=d, w=w, h=h, ref_lin=ref_lin)


def score_png(ctx, out_png):
    return float(sh("ssimulacra2", ctx["ref_lin"], out_png).stdout.strip())


def candidate(ctx, fmt, q):
    """Encode src -> fmt at quality q, decode back via oximg, score."""
    d = ctx["dir"]
    out = d / f"{fmt}_q{q}.{fmt}"
    env = {"OXIMG_WEBP_QUALITY": str(q)} if fmt == "webp" else {"OXIMG_AVIF_QUALITY": str(q)}
    sh(QCLI, "transcode", ctx["src"], MAXW, MAXH, fmt, out, env=env)
    png = d / f"{fmt}_q{q}.png"
    sh(QCLI, "transcode", out, MAXW, MAXH, "png", png)
    return dict(image=ctx["tag"], service=f"oximg-{fmt}", quality=q,
                bytes=out.stat().st_size, ssim2=score_png(ctx, png))


def anchor(ctx):
    """oximg's JPEG-out default (jpegli q80): the same-format baseline."""
    d = ctx["dir"]
    out = d / "jpeg_q80.jpg"
    sh(QCLI, "resize", ctx["src"], MAXW, MAXH, 80, "jpegli", out)
    return dict(image=ctx["tag"], service="oximg-jpeg", quality=80,
                bytes=out.stat().st_size, ssim2=float(
                    sh("ssimulacra2", ctx["ref_lin"], out).stdout.strip()))


def main():
    srcs = sorted((CORPUS / "src").glob("*.jpg"))
    print(f"{len(srcs)} sources; webp={WEBP_QUALITIES} avif={AVIF_QUALITIES}")
    with ThreadPoolExecutor(10) as ex:
        ctxs = list(ex.map(prepare, srcs))
        tasks = [(ctx, "webp", q) for ctx in ctxs for q in WEBP_QUALITIES]
        tasks += [(ctx, "avif", q) for ctx in ctxs for q in AVIF_QUALITIES]
        rows = list(ex.map(lambda t: candidate(*t), tasks))
        rows += list(ex.map(anchor, ctxs))

    out = WORK / "xfmt_results.csv"
    with out.open("w", newline="") as f:
        wr = csv.DictWriter(f, fieldnames=["image", "service", "quality", "bytes", "ssim2"])
        wr.writeheader()
        wr.writerows(rows)
    print(f"wrote {out}")

    print(f"\n{'service':<12} {'q':>3} {'mean bytes':>11} {'mean ssim2':>11}")
    keys = sorted({(r["service"], r["quality"]) for r in rows})
    for svc, q in keys:
        sel = [r for r in rows if r["service"] == svc and r["quality"] == q]
        print(f"{svc:<12} {q:>3} {mean(r['bytes'] for r in sel):>11.0f} "
              f"{mean(r['ssim2'] for r in sel):>11.2f}")


if __name__ == "__main__":
    main()
