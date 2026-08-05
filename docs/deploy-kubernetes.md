# Deploying on Kubernetes

oximg fits Kubernetes without adapters: configuration is entirely
environment variables, `/health` serves as both probe endpoints, the
process is stateless (no writes, no local cache), and SIGTERM starts a
graceful drain — which is exactly the rolling-update lifecycle.

## Example manifest

A starting point, not a prescription — adjust resources and replica
count to your traffic:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: oximg
spec:
  replicas: 2
  selector:
    matchLabels: { app: oximg }
  template:
    metadata:
      labels: { app: oximg }
    spec:
      # SIGTERM -> drain in-flight requests -> exit 0. Give slow
      # encodes room; the kubelet's SIGKILL backstops a hung client.
      terminationGracePeriodSeconds: 30
      containers:
        - name: oximg
          # latest rebuilds on every main push; pin a version tag or digest.
          image: ghcr.io/oximg/oximg:0.8.2
          ports:
            - containerPort: 8081
          env:
            - name: OXIMG_SOURCE_BASE_URL
              value: "https://static.example.com/originals"
            # In memory-tight pods, the decoded-size cap is the lever:
            - name: OXIMG_MAX_SRC_PIXELS
              value: "30000000"
          resources:
            requests:
              cpu: "1"
              memory: 512Mi
            limits:
              # Worker count follows the cgroup CPU quota (Rust's
              # available_parallelism reads it), so an integer limit
              # gives you an exact concurrency budget.
              cpu: "2"
              memory: 1Gi
          readinessProbe:
            httpGet: { path: /health, port: 8081 }
            periodSeconds: 5
          livenessProbe:
            httpGet: { path: /health, port: 8081 }
            periodSeconds: 10
          securityContext:
            # The image already runs as uid 10001; these make the pod
            # spec assert it.
            runAsNonRoot: true
            readOnlyRootFilesystem: true
            allowPrivilegeEscalation: false
            capabilities: { drop: ["ALL"] }
---
apiVersion: v1
kind: Service
metadata:
  name: oximg
spec:
  selector: { app: oximg }
  ports:
    - port: 80
      targetPort: 8081
```

For a local-files deployment instead of a remote origin, mount the
images as a read-only volume at `/images` (the default `IMAGES_DIR`)
— a `ro` PVC, an NFS mount, or an init container that syncs from
object storage.

## Sizing and scaling

- **CPU**: oximg saturates whatever it is given — an internal
  semaphore pins concurrent pixel work to the observed core count, and
  throughput scales close to linearly with cores (see
  [BENCH.md](../BENCH.md)). What "observed" means here is specific and
  worth setting deliberately:

  | you set | oximg permits |
  |---|---|
  | `limits.cpu: 1` | 1 |
  | `limits.cpu: 1500m` | **1** |
  | `limits.cpu: 1900m` | **1** |
  | `limits.cpu: 2` | 2 |
  | `limits.cpu: 2500m` | 2 |
  | `limits.cpu: 500m` | 1 (the floor) |
  | only `requests.cpu`, no limit | the **node's** core count |

  (Measured on cgroup v2 with the released image; confirm any
  deployment with the `oximg_cpu_workers` gauge under
  `OXIMG_METRICS=1`.)

  Three things follow. **It reads `limits.cpu`, not `requests.cpu`** —
  requests becomes `cpu.weight`, a scheduling share with no count in
  it, so there is nothing there to observe; a limit set purely as a
  blast-radius guard silently becomes a concurrency decision.
  **Fractional limits round down**, so `1500m` buys the same single
  permit as `1000m` while costing 50% more — the second permit arrives
  at `2`, not at `1001m`. And **with no limit at all, a pod sizes
  itself to the node**, which on a large node is far more concurrency
  than its share of CPU can serve. CPU-manager `static` policy
  (exclusive cores via cpuset) is also respected, and when both a
  cpuset and a quota apply the smaller wins.

  If `oximg_request_duration_seconds{phase="queue"}` is where your
  latency lives, permits are what to raise — and on Kubernetes that
  means raising `limits.cpu` to the next **whole** number.

  With a **remote source** there is a second lever that costs no CPU.
  A permit is held across the origin fetch, so
  `phase="fetch"` / `phase="process"` is the share of your paid CPU time
  spent waiting on the origin, and raising `OXIMG_WORKERS` above the
  CPU count recovers roughly that share (measured: 43% fetch share ->
  +34% throughput and a 24% cut in p95, at an unchanged 1-CPU quota;
  see [bench/permit-lab](../bench/permit-lab/)). Local-file sources
  have no such share, and there the same change costs ~7%.
- **Memory**: bounded by concurrency × per-request buffers, which
  `OXIMG_MAX_SRC_PIXELS` caps. The 64 MP default admits large
  sources; 30 MP is a sensible cap when your originals are phone
  photos and your pods are small. Measure with your own corpus —
  peak RSS under load is reported in [BENCH.md](../BENCH.md).
- **Horizontal scaling**: the process is stateless, so an HPA on CPU
  works out of the box. Request coalescing is per-pod: behind a plain
  round-robin Service its benefit falls toward zero as replicas
  multiply, and a CDN or caching layer in front (honoring the 1-year
  `Cache-Control`) matters far more than pod-local dedup at scale. If
  you do want coalescing across a scaled Deployment, consistent-hash
  the URL at the ingress so identical requests reach the same pod —
  ingress-nginx: `nginx.ingress.kubernetes.io/upstream-hash-by:
  "$request_uri"`.

## Rolling updates

The graceful-shutdown path is what makes surge/unavailable rollouts
clean: the old pod stops accepting, finishes what it has (AVIF
encodes are the slow tail), and exits 0. Keep
`terminationGracePeriodSeconds` comfortably above your slowest
expected request; the kubelet's SIGKILL after the grace period is the
backstop for a client that never finishes reading.

## What oximg does not ship

No Prometheus metrics endpoint and no structured (JSON) logs — stderr
lines only (`OXIMG_LOG=request` to include successes). If you need
request metrics today, derive them at the ingress/mesh layer. This is
a deliberate PoC-stage boundary; see the README's roadmap.
