# Permit lab

Does oximg's CPU permit bound *CPU work*, or whole requests?

It bounds whole requests: `main.rs` acquires a permit, then the
blocking task does the origin fetch **and** the decode. With remote
sources (`OXIMG_SOURCE_BASE_URL`, `gs://`) the network round trip is
therefore paid while holding a permit — so on a one-CPU quota the core
sits idle during the fetch while other requests queue.

This harness answers the question without implementing the change: at a
fixed 1-CPU budget, a *second* permit should recover throughput if and
only if there is IO inside the permit to hide behind, and the gain
should grow with origin latency. Local-file sources are the control —
pure CPU, where a second permit can only hurt.

## Design

Follows [../METHODOLOGY.md](../METHODOLOGY.md): interleaved A/B in one
window (rule 1), more distinct URLs than concurrent requests so the
coalescer is not what gets measured (rule 2), the **same binary**
toggled by env (rule 3), cpusets pinned (rule 5).

- `origin.py` — file origin with an injectable `LATENCY_MS`, modelling
  the origin RTT. (It overrides `server_bind` to skip `getfqdn()`: that
  reverse lookup hangs for tens of seconds on hosts whose resolver has
  no reverse zone, which presents as "the harness started and every
  request timed out".)
- `burst.py` — burst-shaped load: one file at N different widths fired
  at once, which is what a page's `srcset` does and what the field
  report in issue #20 showed matters. Reports client percentiles plus
  the server-side queue/process split as a delta over the measured
  window, so container cold start is excluded.
- `ab.sh` — one cell: interleaved `OXIMG_WORKERS=A` vs `B` at a fixed
  `--cpus`, server pinned to an exclusive core so the load generator
  cannot steal it. Host networking in *both* arms, so neither carries
  docker-proxy NAT the other does not.
- `sweep.sh` — the latency matrix. `fixtures.sh` builds the sources.

```sh
bash fixtures.sh && bash sweep.sh
```

## Measured, 2026-08-05

starship (Ryzen 7 8745HS, 16T, Arch, cgroup v2), released image
`ghcr.io/oximg/oximg:0.8.1`, `--cpus=1 --cpuset-cpus=0`, burst=8,
12 rounds x 2 interleaved repeats, 2000x1500 JPEG source:

| source | origin RTT | W=1 rps | W=2 rps | throughput | p95 W=1 | p95 W=2 |
|---|---|---|---|---|---|---|
| local (control) | — | 24.2 | 22.6 | **-7%** | 330 ms | 353 ms |
| http | 0 ms | 22.5 | 22.3 | ~0% | 355 ms | 359 ms |
| http | 10 ms | 18.7 | 21.6 | **+15%** | 428 ms | 376 ms |
| http | 30 ms | 13.3 | 18.1 | **+36%** | 604 ms | 448 ms |
| http | 60 ms | 9.6 | 14.5 | **+51%** | 834 ms | 565 ms |

The control behaves exactly as theory says: for pure CPU work a second
permit is neutral-to-negative (fair share doubles `srv_process`,
43 -> 85 ms, and buys nothing), so the gains in the IO cells are not an
artefact of oversubscription being free.

`srv_process` under W=1 rises with origin latency — 44, 52, 75, 102 ms
— which is the fetch wait *inside* the permit, measured directly. At a
60 ms RTT roughly 59% of the permit's time is not CPU work, and
recovering it is worth +51% throughput and a third off p95.

**Scaling caveat.** Service time here is ~43 ms of CPU, so a 60 ms RTT
is 59% waste. A deployment with heavier work dilutes it: at the 138 ms
service time reported in issue #20, a 20 ms RTT is ~14% waste and the
expected `W=2` gain is ~16%, not 51%. Read the ratio, not the headline.

## The fetch share, measured directly (2026-08-05)

`phase="fetch"` landed after the sweep above, so the same cells can now
report the quantity instead of having it inferred from a latency curve.
Same host and settings, `W=1` vs `W=2` interleaved:

| injected RTT | measured `srv_fetch` | `fetch/process` | W=2 throughput |
|---|---|---|---|
| 0 ms | 0.7 ms | 2% | 0% |
| 10 ms | 10.8 ms | 21% | +14% |
| 30 ms | 30.8 ms | 43% | +34% |
| 60 ms | 60.8 ms | 60% | +49% |

Two things worth having in writing. The metric finds each injected
latency to within 0.8 ms, which is the only reason to trust it — a
timing metric nobody checked against a reference is not evidence. And
the share **predicts the effect**: recoverable throughput tracks
`fetch/process` closely enough that a deployment can read its own
number and know what a permit is worth, rather than running this sweep.

## What follows from this

1. `OXIMG_WORKERS` above the CPU count is *correct* for remote-source
   deployments — the opposite of the guidance in issue #10, which was
   about pinning it *below* observed parallelism on quota-scheduled
   platforms. Both are true because the knob is being asked to do two
   different jobs.
2. The implementation fix is to stop holding a permit across the fetch
   (buffer the source, bounded by `OXIMG_MAX_SOURCE_BYTES`, then
   acquire) — worth it exactly to the extent that the fetch fraction is
   large, which is now a measurable quantity rather than a guess.
3. ~~Either way a `phase="fetch"` split would let a deployment read its
   own fetch fraction~~ — done, and it turned out to predict the effect
   size, not just describe the cause. Read `fetch/process` first; if it
   is small, item 2 is not worth implementing for that deployment.
