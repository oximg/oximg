#!/usr/bin/env python3
"""Burst-shaped load generator + server-side metric deltas.

Shaped after what production traffic actually does (issue #20): a page
load fires a whole `srcset` at once and CDN cache-miss storms arrive in
batches, so mean utilisation stays low while the queue tail is set by
bursts. A Poisson generator would measure a different service.

Each burst is `--burst` requests for the SAME file at DIFFERENT widths:
distinct FlightKeys, so request coalescing cannot merge them (bench
methodology rule 2 — otherwise the coalescer is what gets measured).

Reports client-side percentiles and the server-side queue/process split
computed as a delta over the measured window, so container cold start
is excluded.
"""

import argparse
import statistics
import sys
import threading
import time
import urllib.error
import urllib.request


def scrape(base):
    with urllib.request.urlopen(f"{base}/metrics", timeout=10) as r:
        return r.read().decode()


def series(text, prefix):
    for line in text.splitlines():
        if line.startswith(prefix) and line[len(prefix)] in " {":
            return float(line.rsplit(" ", 1)[1])
    return 0.0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--file", required=True)
    ap.add_argument("--burst", type=int, default=8)
    ap.add_argument("--rounds", type=int, default=20)
    ap.add_argument("--warmup", type=int, default=2)
    ap.add_argument("--gap-ms", type=float, default=0.0)
    ap.add_argument("--widths", default="")
    args = ap.parse_args()

    widths = (
        [int(w) for w in args.widths.split(",")]
        if args.widths
        else [320, 480, 640, 750, 828, 1080, 1200, 1920][: args.burst]
    )
    while len(widths) < args.burst:  # more VUs than widths would coalesce
        widths.append(widths[-1] + 7 * (len(widths) + 1))

    def one(width, out):
        url = f"{args.base}/resize/{width}/0/{args.file}"
        t0 = time.perf_counter()
        try:
            with urllib.request.urlopen(url, timeout=120) as r:
                r.read()
                code = r.status
        except urllib.error.HTTPError as e:
            code = e.code
        except Exception:
            code = 0
        out.append((time.perf_counter() - t0, code))

    def burst():
        out = []
        threads = [
            threading.Thread(target=one, args=(w, out)) for w in widths[: args.burst]
        ]
        t0 = time.perf_counter()
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        return out, time.perf_counter() - t0

    for _ in range(args.warmup):
        burst()

    before = scrape(args.base)
    lat, codes, wall = [], [], 0.0
    for _ in range(args.rounds):
        out, elapsed = burst()
        wall += elapsed
        for d, c in out:
            lat.append(d * 1000)
            codes.append(c)
        if args.gap_ms:
            time.sleep(args.gap_ms / 1000.0)
    after = scrape(args.base)

    n = len(lat)
    lat.sort()

    def pct(p):
        return lat[min(n - 1, int(n * p / 100))]

    # Server-side means over the measured window only, so the
    # container's cold start does not land in the average.
    q_sum = series(after, 'oximg_request_duration_seconds_sum{phase="queue"}') - series(
        before, 'oximg_request_duration_seconds_sum{phase="queue"}'
    )
    q_cnt = series(after, 'oximg_request_duration_seconds_count{phase="queue"}') - series(
        before, 'oximg_request_duration_seconds_count{phase="queue"}'
    )
    p_sum = series(after, 'oximg_request_duration_seconds_sum{phase="process"}') - series(
        before, 'oximg_request_duration_seconds_sum{phase="process"}'
    )
    p_cnt = series(after, 'oximg_request_duration_seconds_count{phase="process"}') - series(
        before, 'oximg_request_duration_seconds_count{phase="process"}'
    )
    f_sum = series(after, 'oximg_request_duration_seconds_sum{phase="fetch"}') - series(
        before, 'oximg_request_duration_seconds_sum{phase="fetch"}'
    )
    f_cnt = series(after, 'oximg_request_duration_seconds_count{phase="fetch"}') - series(
        before, 'oximg_request_duration_seconds_count{phase="fetch"}'
    )
    ok = sum(1 for c in codes if c == 200)
    print(
        f"rps={n / wall:6.2f} ok={ok}/{n} "
        f"p50={pct(50):7.1f} p95={pct(95):7.1f} p99={pct(99):7.1f} max={lat[-1]:7.1f} "
        f"srv_queue={1000 * q_sum / max(1, q_cnt):7.1f} "
        f"srv_process={1000 * p_sum / max(1, p_cnt):7.1f} "
        # The share of a held permit during which no byte could be
        # decoded — the quantity that decides whether bounding whole
        # requests instead of CPU work is costing anything.
        f"srv_fetch={1000 * f_sum / max(1, f_cnt):6.1f} "
        f"fetch/process={100 * f_sum / max(1e-9, p_sum):4.0f}% "
        f"workers={series(after, 'oximg_cpu_workers'):.0f}"
    )


if __name__ == "__main__":
    main()
