pub mod pipeline;

pub(crate) mod config;

/// Strict validation of every OXIMG_* runtime knob — the server
/// binary calls this at startup and refuses to boot on a set-but-
/// invalid value. Library embedders may call it to get the same
/// fail-closed behavior; without it, invalid values fall back to
/// defaults.
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
