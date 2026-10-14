# S3 transport harness

A loopback-only S3 test endpoint with fixed, fake credentials. It verifies
SigV4 against the HTTP request it receives, using an implementation independent
of the production signer and checked against AWS's published GET Object vector.
The transport is shared with `tests/server.rs` and `tests/remote_api.rs`.

No AWS account, containers, Python, network downloads or new crates are needed.
This is a fault-injection tool, not a complete S3 emulator: it serves committed
fixtures, supports path-style GETs, and does not model IAM, TLS, uploads, object
versions, credential refresh or clock-skew enforcement.

## Run the contract checks

```sh
cargo build --release --bins --example s3-harness
./target/release/examples/s3-harness check
# Select another compiled checkout (oximg must be beside oximg-ctl):
./target/release/examples/s3-harness check --ctl /path/to/oximg-ctl
```

`check` starts an endpoint, invokes the real `oximg-ctl` and `oximg` binaries,
and prints one JSON object with `ok`, `passed`, `total` and individual `cases`.
Exit 0 means all checks passed; a mismatched status, signature, path, request
count, image dimensions, or failed command produces a nonzero exit. Each case
includes the observed ctl JSON and server stderr for diagnosis. The runner
removes inherited `AWS_*` and `OXIMG_*` values, then supplies fake credentials.

The cases cover success, exact paths for escaped characters, signed session
tokens, source-size caps, decoded key lengths of 1024/1025 bytes, long prefixes,
missing objects, deployment faults, redirects, one retry, exhausted retries,
error-body EOF/timeout/size limits, and a shared retry/body deadline. All 200
images must be JPEG at 100×75. Retry cases check the actual request count.

The same catalog runs automatically in `cargo test --test s3_harness`, in both
default and AVIF CI builds. CI also runs the standalone executable. The server
suite additionally covers exact 16 KiB body boundaries with Content-Length and
chunked framing, startup faults, timeout metrics, cancellation/slot release,
and Unicode prefixes. Both the listener and in-flight workers are stopped on
drop, including deliberately stalled responses.

## Keep an endpoint running

```sh
./target/release/examples/s3-harness serve
```

The ready JSON contains `endpoint`, `source` and an `env` object with all S3
settings. Use those values as repeated `oximg-ctl --env NAME=VALUE` arguments,
and set `OXIMG_SOURCE_BASE_URL=s3://test-bucket`. Set
`OXIMG_UPSTREAM_TIMEOUT=1` for the timeout scenarios. SIGINT or SIGTERM stops
the endpoint cleanly. It always binds an OS-assigned port on `127.0.0.1`.

| Object key | Response |
|---|---|
| `photo.jpg`, any prefix ending in `/photo.jpg` or `/x` | Committed JPEG |
| `space name.jpg`, `plus+sign.jpg`, `100%.jpg`, `中文.jpg`, `a;b=c&d.jpg`, `literal%2E.jpg` | JPEG only at the correct SigV4-encoded path |
| `region.jpg`, `denied.jpg`, `expired.jpg` | Deployment errors |
| `invalid-key.jpg` | Invalid object name |
| `redirect.jpg` | 307 to `photo.jpg` |
| `always503.jpg` | 503 SlowDown on every attempt |
| `flaky.jpg` | First request 503, subsequent requests JPEG |
| `timeout400.jpg` | 400 headers and incomplete body, socket held for 5 seconds |
| `truncated404.jpg` | 404 with fewer bytes than Content-Length |
| `oversized404.jpg` | Chunked 404 body above 16 KiB |
| `retry-headers.jpg`, `retry-body.jpg` | 650 ms first attempt → 503; second attempt delays headers or body another 650 ms |
| Anything else | 404 NoSuchKey, including startup probes |

`s3://no-bucket` answers NoSuchBucket; `s3://denied` answers AccessDenied.
Keys above 1024 decoded bytes are refused. Fault counters are per wire path
and persist for the life of the endpoint; restart `serve` to reset them.
Unsigned or incorrectly signed requests return SignatureDoesNotMatch before
the route runs.

## Add a regression

Use `S3::spawn` with a closure and `Reply` in Rust integration tests. A reply
can contain arbitrary HTTP bytes, timed writes, a delayed header, or a body
shorter than its Content-Length. Add reusable routes and expected ctl results
to `scenarios.rs` so both CI and the standalone checker exercise them. Keep
new failures reproducible against the old implementation before applying a
fix. Use real-store tests separately for provider-specific compatibility.
