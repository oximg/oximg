pub mod pipeline;

// Arch-neutral driver for the SIMD row kernels; gated with its only
// implementor for now (the AVX2 port ungates it).
#[cfg(target_arch = "aarch64")]
pub(crate) mod resize_kernel;

#[cfg(target_arch = "aarch64")]
pub mod resize_neon;

#[cfg(feature = "avif")]
pub mod svt;

#[cfg(feature = "avif")]
pub mod avif;

#[cfg(feature = "avif")]
mod yuv;
