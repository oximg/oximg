pub mod pipeline;

pub(crate) mod config;
pub(crate) mod meta;
pub(crate) mod resize_kernel;

#[cfg(target_arch = "aarch64")]
pub mod resize_neon;

#[cfg(target_arch = "x86_64")]
pub mod resize_avx2;

#[cfg(feature = "avif")]
pub mod svt;

#[cfg(feature = "avif")]
pub mod avif;

#[cfg(feature = "avif")]
mod yuv;
