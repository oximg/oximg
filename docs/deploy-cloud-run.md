# Deploying on Cloud Run (and similar serverless containers)

Cloud Run is a natural fit: oximg reads `PORT` from the environment
(Cloud Run's contract), binds 0.0.0.0, starts in well under a second,
is stateless, and drains gracefully on the SIGTERM Cloud Run sends at
scale-down. The same shape applies to AWS App Runner / Fargate and
Azure Container Apps.

## Source mode: use a remote origin

Serverless containers have no durable local disk, so skip `IMAGES_DIR`
and point `OXIMG_SOURCE_BASE_URL` at where the originals live:

```sh
gcloud run deploy oximg \
  --image=docker.io/oximg/oximg:0.8.0 \
  --set-env-vars=OXIMG_SOURCE_BASE_URL=https://storage.googleapis.com/my-bucket \
  --cpu=2 --memory=1Gi \
  --concurrency=8 \
  --allow-unauthenticated
```

The fetcher sends no credentials and follows no redirects, so the
bucket (or path) must be publicly readable — or front private storage
with something that authenticates on oximg's behalf. Decoding
overlaps the download, so origin latency is partially hidden.

Notes on the flags:

- `--image`: pin a version tag or digest — `latest` rebuilds on every
  push to oximg's main branch.
- No `--port` needed: Cloud Run injects `PORT` (8080 by default) and
  oximg honors it; the image's `EXPOSE 8081` is only a default.

## Concurrency vs. CPU

oximg pins concurrent pixel work to the visible core count internally;
requests beyond that queue in-process (cheaply, on a semaphore).
Recommended shape:

- Set `--concurrency` to roughly **2-4× the vCPU count**. Below that
  you scale out before the instance is fully used; far above it,
  requests queue behind the semaphore and p99 latency grows before
  the autoscaler reacts.
- Default request-based CPU allocation is fine: oximg does no
  background work, so it needs no CPU between requests.
- **Leave `OXIMG_WORKERS` unset here.** Cloud Run's `cpu` is a *time
  quota, not a core count*: a `cpu: "1"` container is scheduled across
  the machine's cores with a one-vCPU budget, and typically observes 2
  CPUs. That is not over-claiming — two operations in flight is how
  the paid quota gets used. Field measurement (issue #10): pinning
  `OXIMG_WORKERS=1` to "match the billed number" cost 17-36%
  throughput across load shapes and worsened mean CPU-permit queue
  wait 23x (375ms vs 16ms). The observed-parallelism default is the
  right answer on this platform; `oximg_cpu_workers`
  (`OXIMG_METRICS=1`) shows what took effect.
- **Sizing is observable, not guesswork**: watch the
  `oximg_request_duration_seconds{phase="queue"}` histogram. Rising
  queue wait under flat processing time means requests are stacking
  behind the CPU permits — lower `--concurrency` or raise CPU before
  p99 does it for you.

## Request coalescing does not span instances

Coalescing merges identical concurrent requests **within one
process**. On Cloud Run every instance has its own in-flight map, so
as the autoscaler fans out, identical requests land on different
instances and the benefit falls toward zero (measured: 681 leaders, 0
followers across 6 instances under duplicate-heavy load). Cloud Run
cannot route by URL (session affinity keys on client IP), so no
configuration restores it. This is fine: if a CDN fronts the service
— which the 1-year `Cache-Control` assumes — the CDN already collapses
duplicate misses, and coalescing is not the feature you are buying.
Size for pipeline throughput (the diverse-URL numbers in
[BENCH.md](../BENCH.md)), not for dedup.

## Shutdown and cold starts

- **Scale-down**: Cloud Run sends SIGTERM and allows ~10s; oximg
  stops accepting, finishes in-flight requests, and exits 0. Keep the
  slowest expected encode (AVIF on large sources) inside that window
  — cap request cost with `OXIMG_MAX_SRC_PIXELS` if needed.
- **Cold start**: startup is env parsing plus a TCP bind (no model
  loading, no cache warm-up), so `--min-instances=0` is usually
  acceptable. Set `--min-instances=1` only if first-hit latency on an
  idle service matters.

## Caching

Every 200 carries `Cache-Control: public, max-age=31536000`. Put
Cloud CDN (or any CDN) in front so repeat URLs never reach the
service — that, not instance count, is the main cost lever. If you
enable `OXIMG_AUTO_FORMAT` (Accept negotiation), confirm the CDN
respects `Vary: Accept`, or prefer explicit `@webp`/`@avif` URLs;
see the README.

## URL signing

Public serverless endpoints are open to resize-parameter abuse
(anyone can request arbitrary dimensions of arbitrary files). Set
`OXIMG_KEY`/`OXIMG_SALT` to require imgproxy-style signed URLs; the
README documents the signature format.
