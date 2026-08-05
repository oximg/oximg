# Permit lab

Does oximg's CPU permit bound *CPU work*, or whole requests?

Through 0.8.x it bounded whole requests: `main.rs` acquired a permit,
then the blocking task did the origin fetch **and** the decode. With
remote sources (`OXIMG_SOURCE_BASE_URL`, `gs://`) the network round
trip was therefore paid while holding a permit — so on a one-CPU quota
the core sat idle during the fetch while other requests queued. This
lab measured that cost, set the falsifiable target for fixing it, and
then measured the fix (issue #22, the last section): permits now bound
CPU work.

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

## Production data point (2026-08-05)

The deployment in issue #20 ran 0.8.2 and measured its own share over 30
minutes of warm traffic, two pods, same-region `gs://` sources over a
warm connection pool:

```
n=1093   fetch/process 47.0%   process 159.9ms   fetch 75.1ms   queue 234.7ms
n=1116   fetch/process 50.9%   process 149.9ms   fetch 76.2ms   queue 280.1ms
```

Their prediction had been that a ~155 ms service time against this
harness's ~43 ms would make the same round trip a much smaller share.
Both premises held — the service time really is 3.6x, the reads really
are same-region — and the conclusion did not follow: **about half of
every held permit is spent with the core idle.** Which lands between the
43% and 60% rows above, i.e. ~+40% recoverable.

Two things that generalise from it:

- Read the share from **warm** traffic. Their first ~100 requests per
  fresh pod read 55-64%, connection and TLS setup inflating it.
- `permits x (1 - fetch/process)` is the CPU needed at saturation. Two
  permits at a 49% share want ~1.02 CPU, so a `limits.cpu: 1` ceiling
  throttles them — the reason the earlier advice here ("more permits at
  an unchanged quota") needed a platform caveat: it holds where the
  limit is the reservation and misleads where it is not.

## The production-equivalent cell (2026-08-05)

The default cells run ~43 ms of CPU; the reporting deployment runs
~155 ms, so a conclusion drawn at the toy scale is not transferable.
The lever that closes the gap is **output width** — encode cost
dominates, and the default burst spreads 320..1920 while production
serves 1920. `photo.jpg` at widths 1200..1900 costs 62.5 ms of CPU, so a
60 ms origin puts the fetch share where production reports it:

```
W=1  lat=60  rps= 8.34  p95=958ms  queue=400ms  process=120ms  fetch=61ms  fetch/process=51%
W=2  lat=60  rps=11.41  p95=712ms  queue=248ms  process=172ms  fetch=61ms  fetch/process=35%
```

| | production | this cell |
|---|---|---|
| `fetch/process` | 47-51% | **51%** |
| `process` | 150-160 ms | 120 ms |
| queue | 235-280 ms | 400 ms (a harsher burst) |

Reproduce it with:

```sh
IMAGE=<tag> SRC=http LATENCY_MS=60 BURST=8 ROUNDS=12 REPEATS=2 \
  FILE=photo.jpg WIDTHS=1200,1300,1400,1500,1600,1700,1800,1900 bash ab.sh
```

**This is the baseline any permit-scoping change has to beat.** The
prediction to test: buffering the source *outside* the permit should let
**one** permit reach the 11.4 rps that two permits reach here, at the
same 1-CPU quota, with `fetch/process` unchanged in the metric (the wait
still happens, it just stops holding a permit). If it does not, the
model is wrong and the change should not land.

## The change, measured (2026-08-05)

Issue #22 implemented (fetch buffered off-permit, `oximg:issue22`),
same cell, same window as a re-run of the pre-change build:

```
pre-change  W=1   8.36 rps  p95 957ms  process=120ms  fetch= 61ms
pre-change  W=2  11.55 rps  p95 703ms  process=171ms  fetch= 61ms
issue #22   W=1  14.95 rps  p95 533ms  process= 62ms  fetch= 94ms
issue #22   W=2  14.74 rps  p95 543ms  process=120ms  fetch= 65ms
```

(The pre-change arm re-ran in the same window and reproduced the
baseline table above to within 0.1 rps — the comparison is not ambient
drift.)

The prediction held with room to spare: one permit did not just reach
the two-permit 11.4 rps, it passed it — because the old W=2 still paid
`permits x (1 - fetch/process)` ≈ 1.3 CPU against a 1-CPU quota, while
the new W=1 runs pure CPU at 62 ms/request (the fixture's measured
encode cost exactly). A second permit now buys nothing, as the local
control always said it should for pure CPU work.

Reading the new metric shape: `process` is the permit's actual hold
(120 → 62 ms at W=1), and `fetch` is a *sibling* phase that includes
waiting for a fetch slot — at W=1 the default `OXIMG_FETCH_CONCURRENCY`
is 4, so an 8-burst fetches in two waves and the mean reads ~94 ms
against a 60 ms RTT; at W=2 (8 slots) one wave, ~65 ms. `fetch/process`
above 100% is therefore expected, not an accounting bug: the wait still
happens, it just no longer holds a permit.

## What follows from this

1. ~~`OXIMG_WORKERS` above the CPU count is *correct* for remote-source
   deployments~~ — was, through 0.8.x (the opposite of the guidance in
   issue #10, which was about pinning it *below* observed parallelism
   on quota-scheduled platforms; both were true because the knob was
   being asked to do two different jobs). With the fetch out of the
   permit the workaround is obsolete: the second permit measured
   *nothing* at the same quota, and the knob goes back to doing one
   job.
2. ~~The implementation fix is to stop holding a permit across the
   fetch~~ — done (issue #22: buffer the source, bounded by
   `OXIMG_MAX_SOURCE_BYTES` and `OXIMG_FETCH_CONCURRENCY`, then
   acquire), and it beat its own target: the fix recovers *more* than a
   second permit did, because it also stops paying the CPU headroom a
   second permit needs at saturation.
3. ~~Either way a `phase="fetch"` split would let a deployment read its
   own fetch fraction~~ — done, and it turned out to predict the effect
   size, not just describe the cause. Read `fetch/process` first; if it
   is small, item 2 is not worth implementing for that deployment.
