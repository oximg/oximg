#!/usr/bin/env python3
"""SSIMULACRA2 quality harness: oximg vs imgproxy vs sharp.

Two test groups:
  A (encoder isolation): identical pre-encode pixels (oximg's resize output),
    encoded by each JPEG encoder. Scored against the pre-encode pixels.
  B (end-to-end): each service resizes+encodes from the same JPEG source.
    Scored against a linear-light Lanczos ground-truth downscale (primary)
    and an sRGB-space Lanczos reference (secondary).

Usage: run.py <workdir> [--imgproxy http://127.0.0.1:8082]
Emits <workdir>/results.csv
"""

import csv
import json
import subprocess
import sys
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
QCLI = REPO / "target/release/examples/qcli"
CORPUS = Path(__file__).resolve().parent / "corpus"
QUALITIES = [60, 70, 75, 80, 85, 90]
MAXW = MAXH = 500

IMGPROXY = "http://127.0.0.1:8082"
for i, a in enumerate(sys.argv):
    if a == "--imgproxy":
        IMGPROXY = sys.argv[i + 1]
WORK = Path(sys.argv[1]).resolve()
WORK.mkdir(parents=True, exist_ok=True)


def sh(*cmd):
    r = subprocess.run([str(c) for c in cmd], capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"{cmd}: {r.stderr.strip()[:300]}")
    return r


def sources():
    out = []
    for sub in ("src", "large"):
        for p in sorted((CORPUS / sub).glob("*.jpg")):
            out.append((f"{sub}/{p.name}", p))
    return out


def prepare(name, src):
    """Master pixels + refs for one source image. Returns per-image context."""
    tag = name.replace("/", "_").removesuffix(".jpg")
    d = WORK / tag
    d.mkdir(exist_ok=True)
    master_ppm, master_png = d / "master.ppm", d / "master.png"
    r = sh(QCLI, "resize", src, MAXW, MAXH, 80, "fast", d / "_master.jpg", master_ppm)
    w, h = map(int, r.stderr.strip().split("x"))
    sh("magick", master_ppm, master_png)
    ref_lin, ref_srgb = d / "ref_linear.png", d / "ref_srgb.png"
    sh("magick", src, "-colorspace", "RGB", "-filter", "Lanczos",
       "-resize", f"{w}x{h}!", "-colorspace", "sRGB", ref_lin)
    sh("magick", src, "-filter", "Lanczos", "-resize", f"{w}x{h}!", ref_srgb)
    return dict(name=name, tag=tag, src=src, dir=d, w=w, h=h,
                master_ppm=master_ppm, master_png=master_png,
                ref_lin=ref_lin, ref_srgb=ref_srgb)


def gen_outputs(ctx):
    """Generate all candidate JPEGs for one image. Returns (rows, sharp_jobs)."""
    d, rows, sharp_jobs = ctx["dir"], [], []
    for q in QUALITIES:
        # --- group A: encoder isolation (from master pixels) ---
        for svc, preset in (("oximg-fast", "fast"), ("oximg-small", "small")):
            out = d / f"A_{svc}_q{q}.jpg"
            sh(QCLI, "encode", ctx["master_ppm"], q, preset, out)
            rows.append(("A", svc, q, out))
        out = d / f"A_turbo_q{q}.jpg"  # plain libjpeg-turbo ~= imgproxy's encoder
        sh("magick", ctx["master_png"], "-sampling-factor", "2x2", "-quality", q, out)
        rows.append(("A", "turbo(magick)", q, out))
        for svc, moz in (("sharp-plain", False), ("sharp-moz", True)):
            out = d / f"A_{svc}_q{q}.jpg"
            sharp_jobs.append(dict(mode="encode", input=str(ctx["master_png"]),
                                   out=str(out), quality=q, mozjpeg=moz))
            rows.append(("A", svc, q, out))
        # --- group B: end-to-end (from JPEG source) ---
        for svc, preset in (("oximg-fast", "fast"), ("oximg-small", "small")):
            out = d / f"B_{svc}_q{q}.jpg"
            sh(QCLI, "resize", ctx["src"], MAXW, MAXH, q, preset, out)
            rows.append(("B", svc, q, out))
        for svc, moz in (("sharp-plain", False), ("sharp-moz", True)):
            out = d / f"B_{svc}_q{q}.jpg"
            sharp_jobs.append(dict(mode="resize", input=str(ctx["src"]), out=str(out),
                                   maxW=MAXW, maxH=MAXH, quality=q, mozjpeg=moz))
            rows.append(("B", svc, q, out))
        out = d / f"B_imgproxy_q{q}.jpg"
        url = (f"{IMGPROXY}/insecure/resize:fit:{MAXW}:{MAXH}/quality:{q}"
               f"/plain/local:///{ctx['name']}")
        with urllib.request.urlopen(url, timeout=30) as resp:
            out.write_bytes(resp.read())
        rows.append(("B", "imgproxy", q, out))
    return rows, sharp_jobs


def score(ctx, group, svc, q, out):
    ident = sh("magick", "identify", "-format", "%w %h", out).stdout.split()
    w, h = int(ident[0]), int(ident[1])
    row = dict(image=ctx["name"], group=group, service=svc, quality=q,
               bytes=out.stat().st_size, w=w, h=h,
               ssim2_master="", ssim2_linear="", ssim2_srgb="")
    if (w, h) != (ctx["w"], ctx["h"]):
        row["dim_mismatch"] = f"{w}x{h} != {ctx['w']}x{ctx['h']}"
        return row
    if group == "A":
        row["ssim2_master"] = sh("ssimulacra2", ctx["master_png"], out).stdout.strip()
    else:
        row["ssim2_linear"] = sh("ssimulacra2", ctx["ref_lin"], out).stdout.strip()
        row["ssim2_srgb"] = sh("ssimulacra2", ctx["ref_srgb"], out).stdout.strip()
    return row


def main():
    srcs = sources()
    print(f"{len(srcs)} sources, qualities={QUALITIES}")
    with ThreadPoolExecutor(10) as ex:
        ctxs = list(ex.map(lambda a: prepare(*a), srcs))
        print("refs ready; generating candidates...")
        gen = list(ex.map(gen_outputs, ctxs))
    all_jobs = [j for _, jobs in gen for j in jobs]
    jobs_file = WORK / "sharp_jobs.json"
    jobs_file.write_text(json.dumps(all_jobs))
    print(f"running {len(all_jobs)} sharp jobs...")
    sh("node", Path(__file__).parent / "sharp_runner.js", jobs_file)
    tasks = [(ctx, g, svc, q, out)
             for ctx, (rows, _) in zip(ctxs, gen) for (g, svc, q, out) in rows]
    print(f"scoring {len(tasks)} outputs...")
    with ThreadPoolExecutor(10) as ex:
        results = list(ex.map(lambda t: score(*t), tasks))
    cols = ["image", "group", "service", "quality", "bytes", "w", "h",
            "ssim2_master", "ssim2_linear", "ssim2_srgb", "dim_mismatch"]
    with open(WORK / "results.csv", "w", newline="") as f:
        wr = csv.DictWriter(f, fieldnames=cols)
        wr.writeheader()
        wr.writerows(results)
    mism = [r for r in results if r.get("dim_mismatch")]
    print(f"done: {len(results)} rows -> {WORK/'results.csv'}; dim mismatches: {len(mism)}")
    for r in mism[:10]:
        print("  MISMATCH", r["image"], r["service"], r["dim_mismatch"])


if __name__ == "__main__":
    main()
