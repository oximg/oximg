#!/usr/bin/env python3
"""Offline libwebp mode experiment; no production knobs or network sources.

Requires release-built gif2webp and ImageMagick. Generates four small synthetic
animations (one uses the committed photo), or accepts --corpus DIR. Reports
interleaved subprocess wall times, bytes, and time-aligned RGBA error against
the decoded GIF. This is not an oximg latency or browser-playback benchmark.
"""

import argparse
import hashlib
import json
import math
import platform
import random
import shutil
import statistics
import subprocess
import time
from pathlib import Path


def run(args):
    return subprocess.run(list(map(str, args)), check=True, capture_output=True).stdout


def generate(root, magick):
    root.mkdir(parents=True, exist_ok=True)
    photo = Path(__file__).resolve().parents[1] / "tests/fixtures/photo.jpg"
    for kind in ("flat", "alpha", "photo-pan", "noise"):
        frames = root / kind
        frames.mkdir(exist_ok=True)
        paths = []
        rng = random.Random(41)
        for i in range(24):
            dst = frames / f"{i:02d}.png"
            if kind == "photo-pan":
                run([magick, photo, "-crop", f"160x120+{i}+{i}", "+repage", dst])
            elif kind == "noise":
                ppm = frames / f"{i:02d}.ppm"
                # Fixed 64-colour palette; avoids palette choice being the
                # dominant source of variation in a deliberately noisy GIF.
                px = bytes(rng.randrange(4) * 85 for _ in range(160 * 120 * 3))
                ppm.write_bytes(b"P6\n160 120\n255\n" + px)
                run([magick, ppm, dst])
            else:
                bg = "none" if kind == "alpha" else "#eef1f5"
                x = 8 + i * 4
                run([magick, "-size", "160x120", f"xc:{bg}", "-fill", "#1864ab",
                     "-draw", f"rectangle {x},30 {x+32},80", "-fill", "#f59f00",
                     "-draw", f"circle {130-i*3},50 {140-i*3},50", dst])
            paths.append(dst)
        run([magick, "-delay", "10", *paths, "-loop", "0", "-layers", "Optimize",
             root / f"{kind}.gif"])
    return sorted(root.glob("*.gif"))


def animation(path, magick):
    rows = run([magick, path, "-coalesce", "-format", "%w %h %T\n", "info:"])
    meta = [tuple(map(int, row.split())) for row in rows.decode().splitlines()]
    raw = run([magick, path, "-coalesce", "-depth", "8", "rgba:-"])
    frames = []
    pos = end = 0
    for w, h, delay in meta:
        if delay <= 1:
            raise ValueError(f"{path}: use >=20ms frame delays for this experiment")
        end += delay * 10
        size = w * h * 4
        frames.append((end, raw[pos:pos + size]))
        pos += size
    if not meta or pos != len(raw) or len({m[:2] for m in meta}) != 1:
        raise ValueError(f"inconsistent coalesced animation: {path}")
    return meta[0][:2], frames


def compare(reference, candidate):
    dims, a = reference
    other_dims, b = candidate
    if dims != other_dims or a[-1][0] != b[-1][0]:
        raise ValueError("canvas or total duration changed")
    # Compare at time intervals, not frame indices: either encoder may merge
    # repeated frames. Ignore RGB behind fully transparent pixels, and compare
    # both premultiplied RGB and alpha so transparency errors remain visible.
    i = j = start = 0
    sse = samples = max_diff = 0
    while i < len(a) and j < len(b):
        end = min(a[i][0], b[j][0])
        duration = end - start
        x, y = a[i][1], b[j][1]
        for k in range(0, len(x), 4):
            ax, ay = x[k + 3], y[k + 3]
            diffs = [(x[k+c] * ax - y[k+c] * ay) / 255 for c in range(3)]
            diffs.append(ax - ay)
            sse += sum(d * d for d in diffs) * duration
            max_diff = max(max_diff, *map(abs, diffs))
        samples += len(x) * duration
        start = end
        if a[i][0] == end:
            i += 1
        if b[j][0] == end:
            j += 1
    return {"rgba_rmse": math.sqrt(sse / samples), "max_diff": max_diff,
            "duration_ms": a[-1][0], "source_frames": len(a), "output_frames": len(b)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=Path("target/gif-modes"))
    parser.add_argument("--corpus", type=Path)
    parser.add_argument("--rounds", type=int, default=7)
    parser.add_argument("--cpu", required=True, help="actual CPU model, recorded verbatim")
    args = parser.parse_args()
    if args.rounds < 3:
        parser.error("at least three interleaved rounds are required")
    gif2webp, magick = shutil.which("gif2webp"), shutil.which("magick")
    if not gif2webp or not magick:
        parser.error("gif2webp and magick must be installed")
    args.out.mkdir(parents=True, exist_ok=True)
    sources = sorted(args.corpus.glob("*.gif")) if args.corpus else generate(args.out / "corpus", magick)
    if not sources:
        parser.error("no GIFs found")
    modes = {"lossy": ["-lossy"], "lossless": [], "mixed": ["-mixed"]}
    result = {"cpu": args.cpu, "platform": platform.platform(), "rounds": args.rounds,
              "gif2webp": run([gif2webp, "-version"]).decode().strip(),
              "magick": run([magick, "-version"]).decode().splitlines()[0],
              "settings": {"q": 75, "method": 2, "kmin": 9, "kmax": 17},
              "generated": args.corpus is None, "cells": []}
    for source in sources:
        commands, times, hashes = {}, {m: [] for m in modes}, {}
        for mode, flags in modes.items():
            out = args.out / f"{source.stem}-{mode}.webp"
            cmd = [gif2webp, "-quiet", *flags, "-q", "75", "-m", "2", "-kmin", "9",
                   "-kmax", "17", "-metadata", "none", source, "-o", out]
            commands[mode] = (cmd, out)
            run(cmd)  # warm every mode before the interleaved measurements
            hashes[mode] = hashlib.sha256(out.read_bytes()).hexdigest()
        for round_no in range(args.rounds):
            order = list(modes)
            random.Random(round_no).shuffle(order)
            for mode in order:
                cmd, out = commands[mode]
                t = time.perf_counter()
                run(cmd)
                times[mode].append((time.perf_counter() - t) * 1000)
                if hashlib.sha256(out.read_bytes()).hexdigest() != hashes[mode]:
                    raise ValueError(f"non-deterministic encode: {source}, {mode}")
        reference = animation(source, magick)
        for mode, (cmd, out) in commands.items():
            quality = compare(reference, animation(out, magick))
            if mode == "lossless" and quality["max_diff"] != 0:
                raise ValueError(f"lossless pixels changed: {source}, {quality}")
            cell = {"source": str(source), "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
                    "source_bytes": source.stat().st_size, "mode": mode,
                    "command": list(map(str, cmd)), "bytes": out.stat().st_size,
                    "sha256": hashes[mode], "samples_ms": times[mode],
                    "median_ms": statistics.median(times[mode]), **quality}
            result["cells"].append(cell)
            print(json.dumps({k: cell[k] for k in ("source", "mode", "bytes", "median_ms", "rgba_rmse")}))
    (args.out / "results.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
