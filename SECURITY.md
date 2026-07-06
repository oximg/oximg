# Security policy

oximg's core job is decoding attacker-controlled images: every request
runs untrusted bytes through four C codec stacks (mozjpeg, libwebp,
dav1d, SVT-AV1) over `unsafe` FFI, plus an in-tree container parser for
AVIF metadata. Memory-safety and denial-of-service issues in that path
are exactly what this policy is here to catch.

## Supported versions

This is a pre-1.0 experimental project. Only the **latest released
version** receives security fixes; please reproduce on it before
reporting. Fixes ship as a new patch release.

## Reporting a vulnerability

**Please do not open a public issue for a vulnerability.** Report it
privately through GitHub: the repository's **Security** tab →
**Report a vulnerability** opens a private advisory only the
maintainers can see. (This requires private vulnerability reporting to
be enabled under Settings → Code security.)

Include enough to reproduce: the oximg version (`oximg --version` or the
crate/image tag), the request, and — crucially — the **input image**
that triggers it (attach it, or a script that generates it). A crash,
hang, or out-of-bounds read on a specific image is the most useful
report we can get.

We aim to acknowledge within **3 business days** and to ship a fix or a
concrete mitigation timeline within **14 days** for confirmed issues.
Coordinated disclosure is welcome; tell us if you have a publication
deadline and we will work to it.

## Scope

In scope — please report:

- memory-safety faults in the decode/encode/resize path (crashes,
  OOB reads/writes, use-after-free), including ones surfaced only
  through the C codecs;
- denial of service from a small input (decompression bombs beyond the
  `OXIMG_MAX_SRC_PIXELS` guard, pathological metadata, unbounded
  allocation or CPU);
- SSRF or path traversal via the remote-source or filename handling;
- signature-verification bypasses when URL signing is enabled.

Out of scope:

- issues requiring a malicious `OXIMG_*` configuration or a hostile
  local filesystem (the operator is trusted);
- missing hardening headers or TLS — oximg expects to run behind a
  proxy that terminates TLS and adds response headers;
- volumetric DoS from request rate alone (rate-limit at the proxy).

## Handling untrusted input safely

If you deploy oximg on untrusted images, the shipped defaults already
help: run the container as its non-root user (the default), keep
`OXIMG_MAX_SRC_PIXELS` / `OXIMG_MAX_SOURCE_BYTES` at sane limits for
your workload, and put it behind a proxy for TLS and rate limiting.
