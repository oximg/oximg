# Deploying on Cloudflare

Two shapes. **Containers** runs the released image unchanged behind a
Worker. **Workers** runs the pipeline compiled to wasm, with no
container, and is experimental. For which to pick, and measured cost
against Cloud Run, see [deploy-targets.md](deploy-targets.md).

Both deploy with the `cf` CLI (verified with 1.0.0-beta.14; it hands the
build to wrangler 4.145.0, the project's pinned devDependency). Use node
22: wrangler exits silently under node 24.

## Containers

[`deploy/cloudflare-containers/`](../deploy/cloudflare-containers/) is a
Worker that owns the container instances through a Durable Object. It
routes each URL path to one of `INSTANCES` instances by hash, so
identical requests still meet in one process and coalesce.

```sh
cd deploy/cloudflare-containers
npm install --ignore-scripts      # sharp (a miniflare dependency) does not build on Intel macOS
OXIMG_SOURCE_BASE_URL=https://images.example.com \
INSTANCES=2 \
cf deploy
```

Deploy-time settings, read by `cloudflare.config.ts`:

| Variable | Default | |
|---|---|---|
| `OXIMG_SOURCE_BASE_URL` | required | `https://…` origin for sources |
| `INSTANCES` | 1 | instances to spread URL paths over; also `maxInstances` |
| `INSTANCE_TYPE` | `standard-2` | 1 vCPU / 6 GiB, the measured size; `basic` and `standard-1` are cheaper and unmeasured |
| `IMAGE` | `docker.io/oximg/oximg:0.18.1` | pin a version, as with any deployment |
| `OXIMG_METRICS` | unset | `1` serves `/metrics`, which sits outside URL signing |

The image comes straight from Docker Hub (`image.reference`), so no
local Docker daemon is needed. The first start after a deploy pulls it:
9.9–18.6 s measured. After that, an instance that has slept starts in
0.9–1.2 s.

### Sleep

An instance stops after `sleepAfter` (3 minutes) without requests, and
stopped instances bill nothing. **Verify that it does sleep.** Read
`containersUsageAdaptiveGroups` (or the dashboard) for minutes with
no usage after traffic stops. With `@cloudflare/containers` 0.3.7, one
instance stayed awake and billed for 25 minutes after a load test
ended with ~16 requests aborted mid-flight. Its Durable Object's alarm
ran every 3 minutes and never logged `Activity expired`.
`isActivityExpired()` keeps renewing while `inflightRequests > 0`, and
those aborts apparently never decremented the counter. This has not
been reproduced in isolation. After the Durable Object was reset by a
redeploy, clean traffic slept on schedule. An awake standard-2 costs
about $41 a month in memory and disk alone.

### Notes

- The Durable Object reset that follows a `cf deploy` landed about
  5 minutes after the deploy. Until it lands, the old code keeps
  serving.
- URL signing (`OXIMG_KEY` / `OXIMG_SALT`) is not wired into this
  config yet. Until it is, the endpoint takes any resize anyone sends.

## Workers (experimental)

[`deploy/cloudflare-workers/`](../deploy/cloudflare-workers/) builds
`oximg::pipeline` to wasm with wasi-sdk for the C codecs. Its README
has the build steps, both packagings (workers-rs, and a JS Worker
around a wasip1 module) and the measurements.

What to expect today:

- Output is byte-identical to native on the test fixtures. Rare 1/255
  differences come from SIMD vs scalar rounding.
- CPU is 3–6× native: 129 / 256 / 685 ms for 1 / 3 / 12 MP sources to
  800 px. The free plan's 10 ms CPU limit is too small.
- A 12 MP source uses 97 MB of the isolate's 128 MB, because the
  streaming resize path has no wasm kernel yet.
- A corrupt JPEG traps the instance (panic=abort), answered as 422 by
  the JS Worker. AVIF and URL signing are not available.
