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
