//! oximg resizes and re-encodes images. The same pipeline runs in three
//! ways: an HTTP server (the `oximg` binary), CLI one-shots
//! (`oximg resize`, `oximg probe`), and this library.
//!
//! The library API is the [`pipeline`] module:
//!
//! - [`pipeline::process`] and [`pipeline::process_path`] take a
//!   [`pipeline::Params`] and return the re-encoded bytes and their
//!   format.
//! - [`pipeline::Params`] also carries per-call overrides for the knobs
//!   that are otherwise process-wide `OXIMG_*` environment variables.
//!   `None` keeps the environment setting, and `Some` wins for that
//!   call, so one process can run different settings side by side.
//! - [`pipeline::probe`] reads only the header, and
//!   [`pipeline::probe_header`] adds the EXIF orientation and the
//!   displayed size. [`pipeline::probe_animation`] reports frame count,
//!   play time and loop count without decoding pixels.
//! - Every entry point returns [`pipeline::Error`]. Match on its
//!   [`kind()`](pipeline::Error::kind), not on the message.
//!   [`pipeline::ErrorKind`] is `#[non_exhaustive]`, so keep a wildcard
//!   arm for kinds added later.
//!
//! # Features
//!
//! - `server` (default): the HTTP server and the remote-source
//!   functions (`fetch_url`, `fetch_gcs`, `fetch_s3`, and the matching
//!   `process_*`). Build with `default-features = false` to drop the
//!   HTTP stack when you only need the pipeline.
//! - `avif`: AVIF decode and encode.
//!
//! The [README](https://github.com/oximg/oximg#readme) covers the
//! server, the CLI and every `OXIMG_*` setting.

mod panic_guard;
pub mod pipeline;

pub(crate) mod clock;
pub(crate) mod config;

/// Strict validation of every OXIMG_* runtime knob — the server
/// binary calls this at startup and refuses to boot on a set-but-
/// invalid value. Library embedders may call it to get the same
/// fail-closed behavior; without it, invalid values fall back to
/// defaults. The one lenient pipeline knob, `OXIMG_PNG_EFFORT`, only
/// warns on stderr here and falls back either way.
pub fn config_validate() -> Result<(), String> {
    config::validate()
}
pub(crate) mod meta;
pub(crate) mod resize_kernel;

// SIMD resize kernels: crate-internal. The `bench-internals` feature
// re-exposes them (still #[doc(hidden)]) for the resize_bench examples
// only — without it the crate's public API is identical on every
// architecture instead of varying with these arch-gated modules.
#[cfg(all(target_arch = "aarch64", feature = "bench-internals"))]
#[doc(hidden)]
pub mod resize_neon;
#[cfg(all(target_arch = "aarch64", not(feature = "bench-internals")))]
pub(crate) mod resize_neon;

#[cfg(all(target_arch = "x86_64", feature = "bench-internals"))]
#[doc(hidden)]
pub mod resize_avx2;
#[cfg(all(target_arch = "x86_64", not(feature = "bench-internals")))]
pub(crate) mod resize_avx2;

// Pregenerated SVT-AV1 bindings — internal to the avif encoder, not a
// public surface.
#[cfg(feature = "avif")]
pub(crate) mod svt;

#[cfg(feature = "avif")]
pub mod avif;

#[cfg(feature = "avif")]
mod yuv;
