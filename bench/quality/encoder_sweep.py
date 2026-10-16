#!/usr/bin/env python3
"""Encoder settings across the mixed corpus (issue #61).

Scores jpegli configurations over the corpus encoder_corpus.py builds:
adaptive quantization on or off, 4:4:4 or 4:2:0 chroma, each written
sequential (SOF1) and progressive (oximg's scan script). The scan
script is a lossless reordering, so the two share one score and only
their bytes and encode time differ.

Every configuration is swept over a range of quality settings, so a
configuration that changes what a given q means (AQ off at the same q
is both larger and better) is compared on its rate-distortion curve,
not at an equal q. Per image, the BD-rate against the shipped default
(progressive, AQ on, 4:4:4) is the mean log-bytes difference over the
overlapping SSIMULACRA2 range; the report gives each class's
geometric mean, median and worst image.

References: the truth PNG at the output's size, Lanczos in linear
light (primary) and in sRGB; at the source's own size, the truth
itself.

The binary needs the experiment knobs OXIMG_X_AQ=0, OXIMG_X_OPT=1
and OXIMG_X_SAMPLING=420: build 57c57e8 with encoder_sweep_knobs.patch
applied (it also adds the ignored `encode_configs_bench`, encoder CPU
per configuration). OXIMG_JPEG_PROGRESSIVE=0 selects sequential. On
later commits sequential output always has optimized tables, so "S"
and "O" coincide.

Results (issue #61, 2026-10-06), BD-rate against progressive AQ-on
4:4:4 with the linear-light reference: AQ off gains 3-5% on photos
but loses against the sRGB reference on gray, text and UI; 4:2:0
loses 15-36% on photos; sequential with fixed tables loses 11-58%,
with optimized tables 1.5-3.7% on photos and 4.5-11.7% on graphics.

  python3 bench/quality/encoder_sweep.py CORPUS --bin BIN
      [--qs 30,40,50,60,70,80,85,90,95] [--jobs N] [--out CORPUS/sweep.json]
  python3 bench/quality/encoder_sweep.py CORPUS --report [--ref lin|srgb]

Needs `magick` and `ssimulacra2` on PATH.
"""

import argparse
import json
import math
import os
import pathlib
import re
import statistics
import subprocess
from concurrent.futures import ProcessPoolExecutor

# (label, env). Scores are taken from the sequential encode.
PIXELS = [
    ("aq-444", {}),
    ("noaq-444", {"OXIMG_X_AQ": "0"}),
    ("aq-420", {"OXIMG_X_SAMPLING": "420"}),
    ("noaq-420", {"OXIMG_X_AQ": "0", "OXIMG_X_SAMPLING": "420"}),
]
# Sequential with jpegli's default fixed Huffman tables, sequential
# with optimized tables, and progressive (always optimized).
SCANS = [
    ("S", {"OXIMG_JPEG_PROGRESSIVE": "0"}),
    ("O", {"OXIMG_JPEG_PROGRESSIVE": "0", "OXIMG_X_OPT": "1"}),
    ("P", {}),
]
BASE = "P/aq-444"
# The encode time, or the fused decode+encode total where the two
# interleave (JPEG sources).
TIMING = re.compile(r"(?:encode|total)=([\d.]+)ms")


def sh(*cmd, env=None):
    r = subprocess.run([str(c) for c in cmd], capture_output=True, text=True, env=env)
    if r.returncode != 0:
        raise RuntimeError(f"{cmd[0]} failed: {r.stderr.strip()[:300]}")
    return r


def dims(path):
    return tuple(int(v) for v in sh("magick", "identify", "-format", "%w %h", path).stdout.split())


def reference(it, w, h, kind, refdir):
    truth = pathlib.Path(it["truth"])
    if dims(truth) == (w, h):
        return truth
    ref = refdir / f"{it['id']}-{w}x{h}-{kind}.png"
    if not ref.exists():
        tmp = ref.with_suffix(f".{os.getpid()}.png")
        lin = ["-colorspace", "RGB"] if kind == "lin" else []
        back = ["-colorspace", "sRGB"] if kind == "lin" else []
        sh("magick", truth, *lin, "-filter", "Lanczos", "-resize", f"{w}x{h}!", *back, tmp)
        os.replace(tmp, ref)
    return ref


def run_item(job):
    it, qs, binary, work = job
    refdir = work / "refs"
    base_env = {k: v for k, v in os.environ.items() if not k.startswith("OXIMG_")}
    base_env["OXIMG_TIMING"] = "1"
    rows = []
    for q in qs:
        for label, extra in PIXELS:
            cell = {}
            for scan, penv in SCANS:
                out = work / "tmp" / f"{it['id']}-{q}-{label}-{scan}.jpg"
                env = dict(base_env, **extra, **penv)
                r = sh(binary, "resize", it["src"], it["target"], 0, out, "-q", q, "-f", "jpg", env=env)
                m = TIMING.search(r.stderr)  # absent on the exact-size path
                ms = float(m[1]) if m else None
                cell[scan] = (out, out.stat().st_size, ms)
            out = cell["S"][0]
            w, h = dims(out)
            scores = {}
            for kind in ("lin", "srgb"):
                ref = reference(it, w, h, kind, refdir)
                scores[kind] = float(sh("ssimulacra2", ref, out).stdout.strip())
            for scan, _ in SCANS:
                rows.append(dict(id=it["id"], cls=it["cls"], q=q, cfg=f"{scan}/{label}",
                                 bytes=cell[scan][1], ms=cell[scan][2], **scores))
                cell[scan][0].unlink()
    return rows


def bd_rate(a, b):
    """Mean log-bytes difference of curve b against a over their common
    score range; a and b are [(score, bytes)] sorted by q. None when
    the ranges do not overlap."""
    def prep(c):
        pts = sorted((s, math.log(n)) for s, n in c)
        # A score can dip as q rises (rare); keep it monotone.
        out = []
        for s, l in pts:
            if out and s <= out[-1][0]:
                continue
            out.append((s, l))
        return out

    a, b = prep(a), prep(b)
    lo, hi = max(a[0][0], b[0][0]), min(a[-1][0], b[-1][0])
    if hi - lo < 1.0:
        return None

    def at(c, s):
        for (s0, l0), (s1, l1) in zip(c, c[1:]):
            if s0 <= s <= s1:
                return l0 + (l1 - l0) * (s - s0) / (s1 - s0)
        return c[-1][1]

    n = 64
    diffs = [at(b, lo + (hi - lo) * i / n) - at(a, lo + (hi - lo) * i / n) for i in range(n + 1)]
    return math.exp(statistics.fmean(diffs)) - 1


def report(rows, ref):
    by = {}
    for r in rows:
        by.setdefault((r["cls"], r["id"], r["cfg"]), []).append(r)
    classes = sorted({r["cls"] for r in rows}, key=lambda c: (c.split("-")[0], -int(c.split("-")[1]) if "-" in c else 0))
    cfgs = [c for c in sorted({r["cfg"] for r in rows}) if c != BASE]
    print(f"BD-rate vs {BASE} ({ref} reference): geomean / median / worst image; "
          f"encode ms ratio at q80 (geomean)")
    print(f"{'class':<11}{'n':>4}  " + "  ".join(f"{c:>24}" for c in cfgs))
    for cls in classes:
        ids = sorted({r["id"] for r in rows if r["cls"] == cls})
        cells = []
        for cfg in cfgs:
            bds, mss = [], []
            for i in ids:
                base, cand = by.get((cls, i, BASE)), by.get((cls, i, cfg))
                bd = bd_rate([(r[ref], r["bytes"]) for r in base], [(r[ref], r["bytes"]) for r in cand])
                if bd is not None:
                    bds.append(bd)
                b80 = [r["ms"] for r in base if r["q"] == 80]
                c80 = [r["ms"] for r in cand if r["q"] == 80]
                if b80 and c80 and b80[0] and c80[0]:
                    mss.append(math.log(max(c80[0], 0.05) / max(b80[0], 0.05)))
            if not bds:
                cells.append(f"{'-':>24}")
                continue
            g = math.exp(statistics.fmean(math.log1p(x) for x in bds)) - 1
            med, worst = statistics.median(bds), max(bds)
            t = math.exp(statistics.fmean(mss)) if mss else float("nan")
            cells.append(f"{g:+6.1%} {med:+6.1%} {worst:+6.1%} {t:4.2f}x")
        print(f"{cls:<11}{len(ids):>4}  " + "  ".join(cells))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("corpus")
    ap.add_argument("--bin")
    ap.add_argument("--qs", default="30,40,50,60,70,80,85,90,95")
    ap.add_argument("--jobs", type=int, default=os.cpu_count())
    ap.add_argument("--out")
    ap.add_argument("--report", action="store_true")
    ap.add_argument("--ref", default="lin", choices=["lin", "srgb"])
    a = ap.parse_args()
    corpus = pathlib.Path(a.corpus).resolve()
    out = pathlib.Path(a.out) if a.out else corpus / "sweep.json"
    if a.report:
        report(json.loads(out.read_text()), a.ref)
        return
    items = json.loads((corpus / "manifest.json").read_text())
    qs = [int(q) for q in a.qs.split(",")]
    for d in ("refs", "tmp"):
        (corpus / d).mkdir(exist_ok=True)
    rows = []
    with ProcessPoolExecutor(a.jobs) as ex:
        jobs = [(it, qs, a.bin, corpus) for it in items]
        for i, r in enumerate(ex.map(run_item, jobs), 1):
            rows.extend(r)
            if i % 50 == 0:
                print(f"{i}/{len(items)} items", flush=True)
    out.write_text(json.dumps(rows))
    report(rows, a.ref)


if __name__ == "__main__":
    main()
