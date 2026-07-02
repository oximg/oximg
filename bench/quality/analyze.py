#!/usr/bin/env python3
"""Aggregate results.csv into rate-quality tables.

For each (group, service): mean SSIMULACRA2 + mean bytes per quality step,
plus per-image interpolated "bytes needed to reach score S" (log-bytes
interpolation), reported as geometric-mean ratio vs a baseline service.
Usage: analyze.py <results.csv> [--group A|B] [--metric ssim2_master|ssim2_linear|ssim2_srgb]
"""

import csv
import math
import sys
from collections import defaultdict

path = sys.argv[1]
rows = [r for r in csv.DictReader(open(path))]


def interp_bytes_at(points, target):
    """points: sorted [(score, bytes)]. Log-linear interpolation of bytes at score."""
    pts = sorted((float(s), b) for s, b in points)
    if target < pts[0][0] or target > pts[-1][0]:
        return None
    for (s1, b1), (s2, b2) in zip(pts, pts[1:]):
        if s1 <= target <= s2:
            if s2 == s1:
                return b1
            t = (target - s1) / (s2 - s1)
            return math.exp(math.log(b1) + t * (math.log(b2) - math.log(b1)))
    return None


def report(group, metric, baseline):
    data = defaultdict(lambda: defaultdict(list))  # svc -> q -> [(score, bytes)]
    per_img = defaultdict(lambda: defaultdict(list))  # svc -> img -> [(score, bytes)]
    for r in rows:
        if r["group"] != group or not r[metric]:
            continue
        sc, by = float(r[metric]), int(r["bytes"])
        data[r["service"]][int(r["quality"])].append((sc, by))
        per_img[r["service"]][r["image"]].append((sc, by))

    print(f"\n## group {group} — metric {metric}\n")
    print("| service | " + " | ".join(f"q{q}" for q in sorted({q for s in data.values() for q in s})) + " |")
    qs = sorted({q for s in data.values() for q in s})
    print("|---|" + "---|" * len(qs))
    for svc in sorted(data):
        cells = []
        for q in qs:
            pts = data[svc].get(q, [])
            if pts:
                ms = sum(p[0] for p in pts) / len(pts)
                mb = sum(p[1] for p in pts) / len(pts) / 1024
                cells.append(f"{ms:.1f} / {mb:.1f}K")
            else:
                cells.append("-")
        print(f"| {svc} | " + " | ".join(cells) + " |")

    print(f"\nbytes-to-reach-score, geometric mean ratio vs **{baseline}** (lower = smaller files):\n")
    targets = [60, 70, 80, 85]
    print("| service | " + " | ".join(f"S={t}" for t in targets) + " |")
    print("|---|" + "---|" * len(targets))
    for svc in sorted(per_img):
        cells = []
        for t in targets:
            ratios = []
            for img, pts in per_img[svc].items():
                a = interp_bytes_at(pts, t)
                b = interp_bytes_at(per_img[baseline].get(img, []), t)
                if a and b:
                    ratios.append(a / b)
            if ratios:
                g = math.exp(sum(math.log(x) for x in ratios) / len(ratios))
                cells.append(f"{(g - 1) * 100:+.1f}% (n={len(ratios)})")
            else:
                cells.append("-")
        print(f"| {svc} | " + " | ".join(cells) + " |")


report("A", "ssim2_master", "turbo(magick)")
report("B", "ssim2_linear", "imgproxy")
report("B", "ssim2_srgb", "imgproxy")
