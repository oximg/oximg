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
          image: ghcr.io/oximg/oximg:0.5.1
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
  semaphore pins concurrent pixel work to the visible core count, and
  the count respects cgroup CPU limits. Whole-number CPU limits give
  predictable behavior; throughput scales close to linearly with
  cores (see [BENCH.md](../BENCH.md)).
- **Memory**: bounded by concurrency × per-request buffers, which
  `OXIMG_MAX_SRC_PIXELS` caps. The 64 MP default admits large
  sources; 30 MP is a sensible cap when your originals are phone
  photos and your pods are small. Measure with your own corpus —
  peak RSS under load is reported in [BENCH.md](../BENCH.md).
- **Horizontal scaling**: the process is stateless, so an HPA on CPU
  works out of the box. Request coalescing is per-pod; a CDN or
  caching layer in front (honoring the 1-year `Cache-Control`)
  matters far more than pod-local dedup at scale.

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
