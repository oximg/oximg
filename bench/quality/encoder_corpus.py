#!/usr/bin/env python3
"""Build the mixed corpus encoder_sweep.py scores (issue #61).

The encoder change in #79 was calibrated on DIV2K photographs at fit
512 and made small and graphic outputs both larger and worse. This
corpus covers what that calibration did not:

- photo-<N>: DIV2K truths served as q92 4:2:0 JPEGs, fit N wide
  (1020, 512, 256, 128);
- gray-<N>: the same truths in grayscale, served as gray PNGs;
- text, chart, ui: synthetic graphics drawn with ImageMagick, in
  several fonts and colour pairs (coloured text is where chroma
  subsampling and quantization show);
- svg: real vector art (logos, icons, illustrations) rasterized over
  white;
- icon: macOS application icons, flattened over white;
- gif: the graphic truths quantized to 256-colour GIFs (the GIF's own
  pixels are the truth);
- fixture: the repo's test fixtures that regressed in #79.

Graphic classes run at their own size and at half width. Each item is
one line of <out>/manifest.json: class, served source, truth PNG and
target width (0 = the source's own size).

  python3 bench/quality/encoder_corpus.py OUT --div2k DIR [--svg-root DIR]
      [--photos 100] [--svgs 30]

Needs `magick` and `rsvg-convert` on PATH.
"""

import argparse
import hashlib
import json
import pathlib
import random
import subprocess
from concurrent.futures import ThreadPoolExecutor

ROOT = pathlib.Path(__file__).resolve().parents[2]
FONTS = [
    "/System/Library/Fonts/Helvetica.ttc",
    "/System/Library/Fonts/Times.ttc",
    "/System/Library/Fonts/Menlo.ttc",
    "/System/Library/Fonts/Supplemental/Georgia.ttf",
]
# (foreground, background)
COLOURS = [
    ("black", "white"), ("white", "#202124"), ("#c5221f", "white"),
    ("#1a73e8", "#fff8c4"), ("#188038", "#fce8e6"), ("#7b1fa2", "#e8f0fe"),
]
WORDS = ("image resize server decode encode quality bytes latency pixel "
         "linear light shrink jpeg webp avif lanczos thumbnail cache edge "
         "request throughput benchmark kernel table scan progressive").split()


def sh(*cmd):
    r = subprocess.run([str(c) for c in cmd], capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"{cmd[0]}: {r.stderr.strip()[:300]}")
    return r


def text_image(rng, out):
    font = rng.choice(FONTS)
    fg, bg = rng.choice(COLOURS)
    w = rng.choice([480, 640, 800, 1200])
    size = rng.choice([12, 14, 16, 20, 28])
    lines = []
    for _ in range(max(4, 360 // (size + 6))):
        lines.append(" ".join(rng.choice(WORDS) for _ in range(w // (size * 5) + 2)))
    h = (size + 6) * len(lines) + 20
    sh("magick", "-size", f"{w}x{h}", f"xc:{bg}", "-font", font, "-pointsize", size,
       "-fill", fg, "-annotate", "+10+%d" % (size + 6), "\n".join(lines),
       "-alpha", "off", "-type", "TrueColor", out)


def chart_image(rng, out):
    w, h = rng.choice([(640, 400), (800, 500), (1000, 600)])
    font = rng.choice(FONTS)
    draw = []
    n = rng.randint(5, 12)
    pal = ["#4285f4", "#ea4335", "#fbbc04", "#34a853", "#ff6d01", "#46bdc6"]
    x0, y0, x1, y1 = 60, 30, w - 20, h - 50
    draw += [f"stroke #888 line {x0},{y1} {x1},{y1}", f"stroke #888 line {x0},{y0} {x0},{y1}"]
    for g in range(1, 5):
        y = y1 - g * (y1 - y0) // 5
        draw.append(f"stroke #ddd line {x0},{y} {x1},{y}")
    bw = (x1 - x0) // n
    if rng.random() < 0.5:
        for i in range(n):
            top = y1 - rng.randint(20, y1 - y0)
            draw.append(f"stroke none fill {pal[i % len(pal)]} rectangle "
                        f"{x0 + i * bw + 6},{top} {x0 + (i + 1) * bw - 6},{y1}")
    else:
        for s in range(rng.randint(1, 3)):
            pts = " ".join(f"{x0 + i * bw + bw // 2},{y1 - rng.randint(20, y1 - y0)}"
                           for i in range(n))
            draw.append(f"stroke {pal[s]} stroke-width 3 fill none polyline {pts}")
    args = ["magick", "-size", f"{w}x{h}", "xc:white"]
    for d in draw:
        args += ["-draw", d]
    args += ["-font", font, "-pointsize", "14", "-fill", "#333"]
    for i in range(n):
        args += ["-annotate", f"+{x0 + i * bw + 4}+{y1 + 20}", rng.choice(WORDS)[:6]]
    args += ["-pointsize", "20", "-annotate", "+70+24", "Requests per second by " + rng.choice(WORDS)]
    sh(*args, "-alpha", "off", "-type", "TrueColor", out)


def ui_image(rng, out):
    w, h = rng.choice([(720, 480), (1024, 640), (1280, 800)])
    font = rng.choice(FONTS[:2])
    accent = rng.choice(["#1a73e8", "#d93025", "#188038", "#9334e6", "#e37400"])
    args = ["magick", "-size", f"{w}x{h}", "gradient:#f8f9fa-#e8eaed",
            "-fill", accent, "-draw", f"rectangle 0,0 {w},56",
            "-fill", "white", "-font", font, "-pointsize", "22",
            "-annotate", "+20+36", "oximg dashboard"]
    y = 90
    while y < h - 80:
        args += ["-fill", "white", "-stroke", "#dadce0",
                 "-draw", f"roundrectangle 20,{y} {w - 20},{y + 70} 8,8",
                 "-stroke", "none", "-fill", "#202124", "-pointsize", "16",
                 "-annotate", f"+40+{y + 30}", " ".join(rng.choice(WORDS) for _ in range(6)),
                 "-fill", "#5f6368", "-pointsize", "13",
                 "-annotate", f"+40+{y + 54}", " ".join(rng.choice(WORDS) for _ in range(10)),
                 "-fill", accent, "-draw", f"roundrectangle {w - 140},{y + 20} {w - 40},{y + 50} 14,14",
                 "-fill", "white", "-pointsize", "14", "-annotate", f"+{w - 120}+{y + 40}", "Open"]
        y += 90
    sh(*args, "-alpha", "off", "-type", "TrueColor", out)


def flatten(src, out, width=None):
    resize = ["-resize", f"{width}x"] if width else []
    sh("magick", src, "-background", "white", "-alpha", "remove", "-alpha", "off",
       *resize, "-type", "TrueColor", out)


def run_all(jobs):
    """Run the jobs in parallel; a failed one only leaves its output
    missing (an SVG rsvg cannot render), which main() checks for."""
    def one(j):
        try:
            j[0](*j[1])
        except RuntimeError as e:
            print(f"skipped: {e}"[:200])
    with ThreadPoolExecutor() as ex:
        list(ex.map(one, jobs))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--div2k", required=True, help="directory of DIV2K PNG truths")
    ap.add_argument("--svg-root", default="/Volumes/DevSSD")
    ap.add_argument("--photos", type=int, default=100)
    ap.add_argument("--svgs", type=int, default=30)
    a = ap.parse_args()
    out = pathlib.Path(a.out).resolve()
    for d in ("src", "truth"):
        (out / d).mkdir(parents=True, exist_ok=True)
    rng = random.Random(61)
    items, jobs = [], []

    def job(fn, *args):
        jobs.append((fn, args))

    photos = sorted(pathlib.Path(a.div2k).glob("*.png"))[: a.photos]
    for p in photos:
        served = out / "src" / f"{p.stem}-q92.jpg"
        job(sh, "magick", p, "-quality", 92, "-sampling-factor", "2x2", served)
        for w in (1020, 512, 256, 128):
            items.append(dict(cls=f"photo-{w}", src=str(served), truth=str(p), target=w))
    for p in photos[: a.photos // 2]:
        g = out / "truth" / f"{p.stem}-gray.png"
        job(sh, "magick", p, "-colorspace", "Gray", "-type", "Grayscale", g)
        for w in (512, 256):
            items.append(dict(cls=f"gray-{w}", src=str(g), truth=str(g), target=w))

    graphics = []
    for kind, fn, n in (("text", text_image, 12), ("chart", chart_image, 10), ("ui", ui_image, 10)):
        for i in range(n):
            t = out / "truth" / f"{kind}-{i:02}.png"
            job(fn, random.Random(f"{kind}{i}"), t)
            graphics.append((kind, t))
    svgs, seen = [], set()
    for s in sorted(pathlib.Path(a.svg_root).rglob("*.svg")):
        try:
            size = s.stat().st_size
        except OSError:
            continue
        if size < 4096 or s.name in seen or "/target/" in str(s):
            continue
        seen.add(s.name)
        svgs.append(s)
    rng.shuffle(svgs)
    for i, s in enumerate(svgs[: a.svgs]):
        raster = out / "truth" / f"svg-{i:02}-raw.png"
        t = out / "truth" / f"svg-{i:02}.png"
        job(lambda s=s, raster=raster, t=t: (sh("rsvg-convert", "-w", 800, "-b", "white", "-o", raster, s),
                                             flatten(raster, t)))
        graphics.append(("svg", t))
    icons = sorted(pathlib.Path("/Applications").glob("*.app/Contents/Resources/*.icns"))
    for i, ic in enumerate(icons[:14]):
        t = out / "truth" / f"icon-{i:02}.png"
        raw = out / "truth" / f"icon-{i:02}-raw.png"
        job(lambda ic=ic, raw=raw, t=t: (
            sh("sips", "-s", "format", "png", "-Z", 512, ic, "--out", raw), flatten(raw, t)))
        graphics.append(("icon", t))

    run_all(jobs)
    jobs.clear()

    for i, (kind, t) in enumerate(graphics):
        if not t.exists():  # an SVG rsvg could not render
            continue
        for target in (0, -2):
            items.append(dict(cls=kind, src=str(t), truth=str(t), target=target))
        if i % 2 == 0:
            gif = out / "src" / f"{t.stem}.gif"
            gtruth = out / "truth" / f"{t.stem}-gif.png"
            job(lambda t=t, gif=gif, gtruth=gtruth: (
                sh("magick", t, "-colors", 256, gif),
                sh("magick", gif, "-type", "TrueColor", gtruth)))
            for target in (0, -2):
                items.append(dict(cls="gif", src=str(gif), truth=str(gtruth), target=target))

    fixtures = ROOT / "tests/fixtures"
    for name in ("still.gif", "gray.png", "rgb.png", "palette.png", "photo.jpg",
                 "tiny.jpg", "cmyk_icc.jpg", "cmyk_sub.jpg", "cmyk_ycck.jpg", "cmyk_t0.jpg"):
        f = fixtures / name
        t = out / "truth" / f"fixture-{f.stem}.png"
        job(sh, "magick", f"{f}[0]", "-colorspace", "sRGB", "-background", "white",
            "-alpha", "remove", "-alpha", "off", "-type", "TrueColor", t)
        items.append(dict(cls="fixture", src=str(f), truth=str(t), target=0))

    run_all(jobs)

    # Half-width targets need each truth's width.
    for it in items:
        if it["target"] == -2:
            w = int(sh("magick", "identify", "-format", "%w", it["truth"]).stdout)
            it["target"] = max(1, w // 2)
        it["id"] = hashlib.sha1(f"{it['cls']}|{it['src']}|{it['target']}".encode()).hexdigest()[:12]
    (out / "manifest.json").write_text(json.dumps(items, indent=1))
    counts = {}
    for it in items:
        counts[it["cls"]] = counts.get(it["cls"], 0) + 1
    print(json.dumps(counts))


if __name__ == "__main__":
    main()
