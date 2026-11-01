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
7. **Cloud instances: at least two per type.** Two fresh c7i.large
   instances (same AMI, CPU model and stepping) measured the same
   images 23% (oximg) and 14% (imgproxy) apart, moving the ratio from
   1.24x to 1.33x (#107). One instance is one draw: report a ratio as
   the range across instances.
8. **Counting tools perturb the comparison.** `perf stat -a -G
   <cgroup>` on the server container cost imgproxy ~8% and oximg ~4%
   of throughput. Take cross-server req/s from runs without it; use
   counted runs to compare builds of the same server.
9. **Saturated cells move on work *removal* only.** Relocating work
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

`oximg-ctl parity` is the same idea between two cargo builds rather
than two images, with no harness: `./target/release/oximg-ctl parity
--against target/agent/oximg` spawns each build's server in turn over
the committed fixtures (every file that probes as an image, at 100x100,
333x0, and 64x64, bare and `@webp`) and compares the bodies' sha256.
Only cells both builds answer with 200 count as compared; the same
error on both sides is reported as skipped. It is what lets the `agent`
profile stand in for release on behaviour; rerun it on both
architectures when the profiles or the SIMD kernels change. Measured
2026-10-14, 0 differing in every run: amd64 with avif, 192 compared;
amd64 default features, 132; aarch64 default features under QEMU on
an AMD Ryzen 7 8745HS host, 132 (AVIF there unchecked: bookworm's
SVT-AV1 is below the 4.1 floor).
