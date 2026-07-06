# Benchmark methodology

Rules distilled from cycles of being burned; the scripts in this
directory encode them.

## The rules

1. **Interleaved same-window A/B.** Ambient load drifts on the scale
   of minutes; sequential "before vs after" runs measure the machine's
   mood, not the change (observed: a +33% latency swing on an
   unchanged binary across windows). Alternate A and B within the same
   window, several rounds (`ab_interleaved.sh`).
2. **URL space ≥ virtual users.** Request coalescing merges identical
   in-flight URLs: with fewer distinct URLs than VUs you benchmark the
   coalescer, not the pipeline (pigeonhole). The stock k6 script
   assigns URLs per-VU; keep it that way.
3. **Same-binary toggle for ~1% attribution.** When an interleaved A/B
   shows a small regression that theory says should be free, run the
   SAME binary with the feature toggled by env (`toggle_ab.sh`). If
   on ≈ off, the gap between binaries is code-layout/ambient noise —
   ±1-2% from adding any code is normal and not worth chasing.
   (Case studies: the EXIF header scan and the ICC scan both measured
   "-1.3%" binary-vs-binary and 0% toggled.)
4. **One compose owner at a time.** The harness mutates
   docker-compose.yml in place (cpuset, image tag, dataset volume);
   concurrent runs corrupt each other. Serialize.
5. **Pin cpusets.** All services on the same cpuset per cell
   (`0,1` = one SMT pair for the saturated 2-VU cells); topology is
   part of the workload.
6. **Expect outliers.** A single 5-10% low round happens (ambient);
   two more rounds resolve it. Never conclude from one round.
7. **Saturated cells move on work *removal* only.** Relocating work
   between threads changes latency, not saturated throughput; don't
   expect throughput wins from overlap changes.

## Prerequisites

The competitive cells assume the imgproxy benchmark harness
(https://github.com/imgproxy/image-servers-benchmark) checked out at
`$HARNESS_DIR` with `image-servers-benchmark.patch` from this
directory applied (adds `OUT_FORMAT` for cross-format cells), its
`dataset/` populated, and the oximg image tagged per script. Scripts
take the harness location from `HARNESS_DIR` (default
`~/xfmt-harness`).

## Byte-identity gate (`bytecmp.sh`)

Before trusting any perf comparison across builds, prove the outputs
are the same bytes for metadata-free sources: 18 URLs (3 sources × 4
same-format cells + 6 cross-format) hashed across two images. This
doubles as the no-regression gate for refactors — the whole 0.4.x
refactor series shipped under it.
