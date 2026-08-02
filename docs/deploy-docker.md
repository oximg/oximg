# Deploying with Docker

The images on GHCR (`ghcr.io/oximg/oximg`) and Docker Hub
(`docker.io/oximg/oximg`) are multi-arch (linux/amd64 + linux/arm64),
include AVIF, run as an unprivileged user (uid 10001), and carry a
built-in `HEALTHCHECK` against `/health`.

## Pin a version

`latest` rebuilds on **every push to main** — it is a moving target
suitable for trying oximg out, not for production. Tagged releases
publish immutable version tags; pin one (or a digest):

```sh
docker pull ghcr.io/oximg/oximg:0.7.0          # version tag
docker pull ghcr.io/oximg/oximg@sha256:...      # or stronger: a digest
```

## Local images directory

```sh
docker run -d --name oximg \
  -p 8081:8081 \
  -v /srv/images:/images:ro \
  ghcr.io/oximg/oximg:0.7.0
curl "localhost:8081/resize/500/500/photo.jpg" -o out.jpg
```

Mount the source directory read-only: the server never writes to it,
and a `ro` mount makes that a guarantee instead of a convention.

## Remote origin instead of a volume

Set `OXIMG_SOURCE_BASE_URL` and sources are fetched from
`<base>/<file>` over HTTP(S) (rustls; redirects are refused by design —
point the base directly at the right host). `<file>` may span
directories, so an existing bucket or CDN layout
(`/resize/640/480/albums/2026/photo.jpg`) is addressable as-is:

```sh
docker run -d --name oximg -p 8081:8081 \
  -e OXIMG_SOURCE_BASE_URL=https://static.example.com/originals \
  ghcr.io/oximg/oximg:0.7.0
```

The fetcher sends no credentials, so the origin must be reachable
without authentication (public bucket, internal service, or a URL
that embeds its own auth).

## docker-compose

```yaml
services:
  oximg:
    image: ghcr.io/oximg/oximg:0.7.0
    ports:
      - "8081:8081"
    volumes:
      - /srv/images:/images:ro
    environment:
      QUALITY: "80"
      OXIMG_MAX_SRC_PIXELS: "30000000"  # tighten in memory-constrained deploys
    restart: unless-stopped
```

All configuration is environment variables — see the README's
environment-variable list for the full knob inventory (quality,
formats, limits, URL signing, `Accept` negotiation).

## Stopping and upgrading

`docker stop` sends SIGTERM; oximg stops accepting connections,
finishes in-flight requests, and exits 0. Docker escalates to SIGKILL
after `-t` seconds (default 10) — leave that at 10s or higher so slow
encodes (AVIF especially) can drain:

```sh
docker stop -t 15 oximg
```

## Building your own image

The repo's `Dockerfile` needs no host dependencies (it compiles a
pinned SVT-AV1 internally) and accepts codegen tuning for
single-machine deploys:

```sh
docker build -t oximg --build-arg RUSTFLAGS="-C target-cpu=native" .
```

## Operational notes

- **Logging**: failures always log one structured line to stderr;
  `OXIMG_LOG=request` also logs successes. There is no log file —
  collect the container's stderr.
- **Caching**: every 200 carries `Cache-Control: public,
  max-age=31536000`. Put a CDN or caching proxy in front; oximg
  itself does not cache results (concurrent identical requests are
  coalesced instead). If you enable `OXIMG_AUTO_FORMAT`, make sure
  the cache honors `Vary: Accept` — see the README.
- **Memory**: per-request decode memory is bounded by
  `OXIMG_MAX_SRC_PIXELS` (default 64,000,000 px) and concurrency is
  pinned to the core count. On small instances, lowering the pixel
  cap is the effective memory lever.
