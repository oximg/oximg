#!/usr/bin/env python3
"""Native GIF mode study. Build --release --features bench-internals --example gif_native.

Times the real animation loop in isolated processes, interleaved by mode.
Reference capture, WebP decoding and quality scoring happen after all timings.
No experimental codec is exposed through the server or normal CLI.
"""

import argparse
import bisect
import hashlib
import json
import os
import platform
import random
import runpy
import shutil
import statistics
import struct
import subprocess
from pathlib import Path

HELPERS = runpy.run_path(str(Path(__file__).with_name("gif-modes.py")))


def run(argv, **kwargs):
    return subprocess.run(list(map(str, argv)), check=True, capture_output=True, **kwargs).stdout


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read_frames(path):
    data = path.read_bytes()
    if data[:8] != b"OXGIFR01":
        raise ValueError(f"invalid frame capture: {path}")
    w, h, loops, count, duration = struct.unpack_from("<5I", data, 8)
    size, offset, frames, starts = w * h * 4, 28, [], []
    if not w or not h or not count or not duration or len(data) != 28 + count * (size + 4):
        raise ValueError(f"invalid capture size: {path}")
    for _ in range(count):
        starts.append(struct.unpack_from("<I", data, offset)[0])
        frames.append(data[offset + 4:offset + 4 + size])
        offset += size + 4
    if starts[0] != 0 or any(a >= b for a, b in zip(starts, starts[1:] + [duration])):
        raise ValueError(f"invalid timeline: {path}")
    return {"dims": (w, h), "loops": loops, "starts": starts, "duration": duration,
            "frames": frames, "comparison": ((w, h), list(zip(starts[1:] + [duration], frames)))}


def perceptual(ref, out, score_bin, magick, tmp):
    # Five matching timestamps, rendered on both black and white. Alpha is
    # checked separately by RGBA RMSE; SSIMULACRA2 measures displayed RGB.
    scores = []
    for timestamp in [ref["duration"] * i // 5 for i in range(5)]:
        for background in ("black", "white"):
            paths = []
            for name, obj in (("ref", ref), ("out", out)):
                idx = bisect.bisect_right(obj["starts"], timestamp) - 1
                path = tmp / f"{name}.png"
                w, h = obj["dims"]
                run([magick, "-size", f"{w}x{h}", "-depth", "8", "rgba:-",
                     "-background", background, "-alpha", "remove", "-alpha", "off", path],
                    input=obj["frames"][idx])
                paths.append(path)
            scores.append(float(run([score_bin, *paths]).decode().strip()))
    return {"ssimulacra2_mean": statistics.mean(scores), "ssimulacra2_min": min(scores)}


def generate(root, magick):
    HELPERS["generate"](root, magick)
    sources = [root / f"{name}.gif" for name in ("flat", "alpha", "photo-pan", "noise")]
    # Add larger working sets and thin UI edges; generated here once, then
    # copy these exact GIFs between architectures (source hashes are recorded).
    for kind, size in (("large-noise", 512), ("large-alpha", 512), ("ui", 640)):
        frames = root / kind
        frames.mkdir(exist_ok=True)
        paths, rng = [], random.Random(183)
        height = size if kind != "ui" else 360
        for i in range(12):
            path = frames / f"{i:02d}.png"
            if kind == "large-noise":
                px = bytes(rng.randrange(4) * 85 for _ in range(size * height * 3))
                run([magick, "ppm:-", path], input=f"P6\n{size} {height}\n255\n".encode() + px)
            else:
                bg = "none" if kind == "large-alpha" else "#161b22"
                draw = [magick, "-size", f"{size}x{height}", f"xc:{bg}", "-fill", "#58a6ff"]
                if kind == "large-alpha":
                    x = 20 + i * 20
                    draw += ["-draw", f"circle {x+60},150 {x+120},150"]
                else:
                    # Bitmap digits need no platform font; one-pixel strokes
                    # exercise the UI/chroma edge case reproducibly.
                    glyphs = ("111101101101111", "010110010010111", "111001111100111")
                    for line in range(12):
                        for col in range(28):
                            glyph = glyphs[(line + col + i) % 3]
                            for bit, on in enumerate(glyph):
                                if on == "1":
                                    x, y = 12 + col * 20 + bit % 3 * 2, 12 + line * 26 + bit // 3 * 2
                                    draw += ["-draw", f"rectangle {x},{y} {x+1},{y+1}"]
                    draw += ["-draw", f"rectangle 12,334 {40+i*42},343"]
                run([*draw, path])
            paths.append(path)
        dest = root / f"{kind}.gif"
        run([magick, "-delay", "10", *paths, "-loop", "0", "-layers", "Optimize", dest])
        sources.append(dest)
    return sorted(sources)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--bin", type=Path, default=Path("target/release/examples/gif_native"))
    p.add_argument("--out", type=Path, default=Path("target/gif-native"))
    p.add_argument("--corpus", type=Path)
    p.add_argument("--cpu", required=True)
    p.add_argument("--rounds", type=int, default=7)
    p.add_argument("--boxes", default="0x0,128x128,64x64")
    p.add_argument("--ssimulacra2", default=shutil.which("ssimulacra2"))
    p.add_argument("--generate-only", action="store_true")
    args = p.parse_args()
    if args.rounds < 3:
        p.error("at least three interleaved rounds required")
    magick = shutil.which("magick")
    if not magick:
        p.error("ImageMagick required")
    args.out.mkdir(parents=True, exist_ok=True)
    sources = sorted(args.corpus.glob("*.gif")) if args.corpus else generate(args.out / "corpus", magick)
    if not sources:
        p.error("no GIF sources")
    if args.generate_only:
        print(json.dumps({"sources": [str(s) for s in sources]}))
        return
    # Never inherit a production policy or timing logger into measurements.
    env = {k: v for k, v in os.environ.items() if not k.startswith("OXIMG_")}
    boxes = [tuple(map(int, b.split("x"))) for b in args.boxes.split(",")]
    if any(len(b) != 2 or min(b) < 0 for b in boxes):
        p.error("boxes must be nonnegative WxH pairs")
    source_files = ["Cargo.lock", "src/pipeline/mod.rs", "src/pipeline/encode.rs",
                    "src/pipeline/gif.rs", "src/pipeline/gif_bench.rs", "bench/tools/gif_native.rs"]
    result = {"cpu": args.cpu, "platform": platform.platform(), "rounds": args.rounds,
              "source_fingerprints": {f: sha(Path(f)) for f in source_files},
              "rustc": run(["rustc", "--version"]).decode().strip(),
              "binary_sha256": sha(args.bin), "head": run(["git", "rev-parse", "HEAD"]).decode().strip(),
              "tree_dirty": bool(run(["git", "status", "--porcelain"]).strip()),
              "settings": "oximg defaults: q75, method2, production keyframe settings; exact=1 for lossless/mixed",
              "timing": "fresh-process pipeline time, excludes file IO; warmed OS caches; no p95 inference",
              "cells": [], "skipped": []}
    pending = []
    for source in sources:
        for w, h in boxes:
            folder = args.out / f"{source.stem}-{w}x{h}"
            folder.mkdir(exist_ok=True)
            samples, hashes = {m: [] for m in ("lossy", "lossless", "mixed")}, {}
            def encode(mode):
                return json.loads(run([args.bin.resolve(), source.resolve(), w, h, mode, folder / f"{mode}.webp"], env=env))
            try:
                for mode in samples:
                    encode(mode)
                    hashes[mode] = sha(folder / f"{mode}.webp")
            except subprocess.CalledProcessError as error:
                if b"animation declined:" not in error.stderr:
                    raise
                result["skipped"].append({"source": source.name, "box": [w, h],
                                          "reason": error.stderr.decode().strip()})
                continue
            for round_no in range(args.rounds):
                order = list(samples)
                random.Random(round_no).shuffle(order)
                for mode in order:
                    report = encode(mode)
                    assert sha(folder / f"{mode}.webp") == hashes[mode], "nondeterministic output"
                    samples[mode].append(report)
            for mode, rows in samples.items():
                cell = {"source": source.name, "source_sha256": sha(source), "source_bytes": source.stat().st_size,
                        "box": [w, h], "mode": mode, "sha256": hashes[mode], "bytes": rows[0]["bytes"],
                        "median_ms": statistics.median(r["pipeline_ms"] for r in rows),
                        "peak_rss_bytes": max(r["peak_rss_bytes"] for r in rows), "samples": rows}
                result["cells"].append(cell)
                pending.append((cell, source, folder))
            print(f"measured {source.name} {w}x{h}", flush=True)
    # Quality work cannot perturb interleaved timings, or contaminate RSS.
    (args.out / "measurements.json").write_text(json.dumps(result, indent=2) + "\n")
    references = {}
    for cell, source, folder in pending:
        if folder not in references:
            refpath = folder / "reference.raw"
            run([args.bin.resolve(), source.resolve(), *cell["box"], "reference", refpath], env=env)
            references[folder] = read_frames(refpath)
        ref = references[folder]
        raw = folder / f"{cell['mode']}.raw"
        run([args.bin.resolve(), "decode", folder / f"{cell['mode']}.webp", raw], env=env)
        out = read_frames(raw)
        assert ref["loops"] == out["loops"], "loop count changed"
        quality = HELPERS["compare"](ref["comparison"], out["comparison"])
        if cell["mode"] == "lossless":
            assert quality["max_diff"] == 0, (cell, quality)
        if args.ssimulacra2:
            quality.update(perceptual(ref, out, args.ssimulacra2, magick, folder))
        cell["quality"] = quality
        cell["loop_count"] = out["loops"]
        print(json.dumps({k: cell[k] for k in ("source", "box", "mode", "bytes", "median_ms", "peak_rss_bytes", "quality")}), flush=True)
    (args.out / "results.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
