//! Every runtime knob the pipeline reads, resolved once at first use
//! and cached for the process lifetime. One inventory, one caching
//! rule ("set at startup"), typed fields instead of scattered string
//! compares — and a test pinning every knob to its README entry.
//!
//! (Server-startup settings like `PORT`, `IMAGES_DIR`, `OXIMG_KEY`
//! live in `main.rs`, which already reads them exactly once.)

use std::sync::OnceLock;

pub(crate) struct Config {
    /// OXIMG_TIMING: per-stage eprintln timing lines.
    pub timing: bool,
    /// OXIMG_RESIZE=srgb disables the linear-light resize path.
    pub linear_light: bool,
    /// OXIMG_RESIZE_BACKEND=fir: the portable fallback kernel (also
    /// disables fusing, whose workers run the in-tree kernel).
    pub fir_backend: bool,
    /// OXIMG_AUTO_ROTATE ("0" disables).
    pub auto_rotate: bool,
    /// OXIMG_ICC ("0" strips profiles instead of passing them through).
    pub icc_passthrough: bool,
    /// OXIMG_DCT_MARGIN: decode-size headroom over the target.
    pub dct_margin: f64,
    /// OXIMG_JPEG_PROGRESSIVE ("0" selects baseline jpegli).
    pub jpegli_progressive: bool,
    /// OXIMG_FLATTEN_BG: alpha→JPEG flatten background, RRGGBB hex.
    pub flatten_bg: [u8; 3],
    /// OXIMG_PNG_EFFORT: fastest / fast (default) / balanced / high.
    pub png_compression: png::Compression,
    /// OXIMG_WEBP_QUALITY.
    pub webp_quality: f32,
    /// OXIMG_WEBP_EFFORT (libwebp `method`, clamped 0-6 at use).
    pub webp_effort: i32,
    /// OXIMG_WEBP_DECODE_THREADS ("0" disables libwebp's 2-thread
    /// decode pipelining).
    pub webp_decode_threads: bool,
    /// OXIMG_AVIF_QUALITY (libavif semantics).
    #[cfg(feature = "avif")]
    pub avif_quality: u8,
    /// OXIMG_AVIF_ALPHA_QUALITY (defaults to the color quality).
    #[cfg(feature = "avif")]
    pub avif_alpha_quality: Option<u8>,
    /// OXIMG_AVIF_SPEED: SVT preset.
    #[cfg(feature = "avif")]
    pub avif_speed: i8,
    /// OXIMG_AVIF_DECODE_THREADS: dav1d workers. Arch-aware default:
    /// 2 on x86-64 (SMT absorbs the second thread), 1 elsewhere.
    #[cfg(feature = "avif")]
    pub avif_decode_threads: std::os::raw::c_int,
    /// OXIMG_MAX_SOURCE_BYTES: remote-source download cap. Read only
    /// on the remote-source path, which is behind the `server` feature.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub max_source_bytes: u64,
    /// OXIMG_MAX_SRC_PIXELS: decoded-size cap (w*h), enforced after
    /// each format's header parse and before any pixel-sized
    /// allocation — compressed-size caps do not bound decoded size.
    pub max_src_pixels: u64,
}

/// The knob inventory, pinned to the README by `knobs_are_documented`.
#[cfg(test)]
const KNOBS: &[&str] = &[
    "OXIMG_TIMING",
    "OXIMG_RESIZE",
    "OXIMG_RESIZE_BACKEND",
    "OXIMG_AUTO_ROTATE",
    "OXIMG_ICC",
    "OXIMG_DCT_MARGIN",
    "OXIMG_JPEG_PROGRESSIVE",
    "OXIMG_FLATTEN_BG",
    "OXIMG_PNG_EFFORT",
    "OXIMG_WEBP_QUALITY",
    "OXIMG_WEBP_EFFORT",
    "OXIMG_WEBP_DECODE_THREADS",
    "OXIMG_AVIF_QUALITY",
    "OXIMG_AVIF_ALPHA_QUALITY",
    "OXIMG_AVIF_SPEED",
    "OXIMG_AVIF_DECODE_THREADS",
    "OXIMG_MAX_SOURCE_BYTES",
    "OXIMG_MAX_SRC_PIXELS",
    "OXIMG_OVERLAP",
];

fn parsed<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

/// Strict startup validation for the server binary: every knob that
/// is *set* must parse and sit in range — a typo in a limit must not
/// silently fail open to a default (the fail-closed precedent set by
/// the signing config). The library-facing `config()` stays lenient
/// so embedding never aborts a host process over env noise.
pub(crate) fn validate() -> Result<(), String> {
    fn set(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.trim().is_empty())
    }
    fn num<T: std::str::FromStr + PartialOrd + Copy + std::fmt::Display>(
        name: &str,
        lo: T,
        hi: T,
    ) -> Result<(), String> {
        if let Some(v) = set(name) {
            let parsed: T = v
                .trim()
                .parse()
                .map_err(|_| format!("{name}={v:?} is not a valid number"))?;
            if parsed < lo || parsed > hi {
                return Err(format!("{name}={v:?} is out of range ({lo}..={hi})"));
            }
        }
        Ok(())
    }
    fn one_of(name: &str, allowed: &[&str]) -> Result<(), String> {
        if let Some(v) = set(name)
            && !allowed.contains(&v.trim())
        {
            return Err(format!("{name}={v:?} must be one of {allowed:?}"));
        }
        Ok(())
    }
    // Booleans only accept 0/1 — "false" reading as *enabled* is the
    // trap this exists to catch.
    for b in [
        "OXIMG_AUTO_ROTATE",
        "OXIMG_ICC",
        "OXIMG_JPEG_PROGRESSIVE",
        "OXIMG_WEBP_DECODE_THREADS",
    ] {
        one_of(b, &["0", "1"])?;
    }
    one_of("OXIMG_OVERLAP", &["0", "1", "auto"])?;
    one_of("OXIMG_RESIZE", &["srgb", "linear"])?;
    one_of("OXIMG_RESIZE_BACKEND", &["fir", "kernel"])?;
    one_of("OXIMG_PNG_EFFORT", &["fastest", "fast", "balanced", "high"])?;
    one_of("OXIMG_LOG", &["error", "request"])?;
    num("OXIMG_DCT_MARGIN", 1.0f64, 8.0)?;
    num("OXIMG_WEBP_QUALITY", 0.0f32, 100.0)?;
    num("OXIMG_WEBP_EFFORT", 0i64, 6)?;
    num("OXIMG_AVIF_QUALITY", 0i64, 100)?;
    num("OXIMG_AVIF_ALPHA_QUALITY", 0i64, 100)?;
    num("OXIMG_AVIF_SPEED", 0i64, 13)?;
    num("OXIMG_AVIF_DECODE_THREADS", 1i64, 64)?;
    num("OXIMG_MAX_SOURCE_BYTES", 1u64, u64::MAX)?;
    num("OXIMG_MAX_SRC_PIXELS", 1u64, u64::MAX)?;
    if let Some(v) = set("OXIMG_FLATTEN_BG") {
        let t = v.trim().trim_start_matches('#');
        if t.len() != 6 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("OXIMG_FLATTEN_BG={v:?} must be RRGGBB hex"));
        }
    }
    Ok(())
}

pub(crate) fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| Config {
        timing: std::env::var("OXIMG_TIMING").is_ok(),
        linear_light: std::env::var("OXIMG_RESIZE").as_deref() != Ok("srgb"),
        fir_backend: std::env::var("OXIMG_RESIZE_BACKEND").as_deref() == Ok("fir"),
        auto_rotate: std::env::var("OXIMG_AUTO_ROTATE").as_deref() != Ok("0"),
        icc_passthrough: std::env::var("OXIMG_ICC").as_deref() != Ok("0"),
        dct_margin: parsed("OXIMG_DCT_MARGIN").unwrap_or(1.7),
        jpegli_progressive: std::env::var("OXIMG_JPEG_PROGRESSIVE").as_deref() != Ok("0"),
        flatten_bg: std::env::var("OXIMG_FLATTEN_BG")
            .ok()
            .and_then(|v| {
                let v = v.trim().trim_start_matches('#');
                // is_ascii keeps the byte-offset slicing below from
                // panicking on multi-byte values; malformed input falls
                // back to white either way.
                if v.len() != 6 || !v.is_ascii() {
                    return None;
                }
                let c = |i| u8::from_str_radix(&v[i..i + 2], 16).ok();
                Some([c(0)?, c(2)?, c(4)?])
            })
            .unwrap_or([255, 255, 255]),
        png_compression: match std::env::var("OXIMG_PNG_EFFORT").as_deref() {
            Ok("fastest") => png::Compression::Fastest,
            // Balanced spends ~15ms/request more than Fast to shave
            // ~14% of the file; Fast still undercuts libvips' default
            // output size.
            Ok("balanced") => png::Compression::Balanced,
            Ok("high") => png::Compression::High,
            _ => png::Compression::Fast,
        },
        webp_quality: parsed("OXIMG_WEBP_QUALITY").unwrap_or(75.0),
        webp_effort: parsed("OXIMG_WEBP_EFFORT").unwrap_or(2),
        webp_decode_threads: std::env::var("OXIMG_WEBP_DECODE_THREADS").as_deref() != Ok("0"),
        #[cfg(feature = "avif")]
        avif_quality: parsed("OXIMG_AVIF_QUALITY").unwrap_or(55),
        #[cfg(feature = "avif")]
        avif_alpha_quality: parsed("OXIMG_AVIF_ALPHA_QUALITY"),
        #[cfg(feature = "avif")]
        avif_speed: parsed("OXIMG_AVIF_SPEED").unwrap_or(8),
        #[cfg(feature = "avif")]
        avif_decode_threads: parsed("OXIMG_AVIF_DECODE_THREADS")
            .unwrap_or(if cfg!(target_arch = "x86_64") { 2 } else { 1 }),
        max_source_bytes: parsed("OXIMG_MAX_SOURCE_BYTES").unwrap_or(64 * 1024 * 1024),
        max_src_pixels: parsed("OXIMG_MAX_SRC_PIXELS").unwrap_or(64_000_000),
    })
}

#[cfg(test)]
mod tests {
    use super::KNOBS;

    /// Every knob in the inventory must appear in the README, and
    /// every OXIMG_* the crate reads must be in the inventory — the
    /// config is the canonical list.
    #[test]
    fn knobs_are_documented() {
        let readme = include_str!("../README.md");
        for k in KNOBS {
            assert!(readme.contains(k), "{k} is not documented in README.md");
        }
        // Inventory completeness: scan our own sources for env reads.
        let sources = [
            include_str!("config.rs"),
            include_str!("pipeline/mod.rs"),
            include_str!("pipeline/jpeg.rs"),
            include_str!("pipeline/fuse.rs"),
            include_str!("pipeline/formats.rs"),
            include_str!("pipeline/encode.rs"),
            #[cfg(feature = "avif")]
            include_str!("avif/encode.rs"),
            #[cfg(feature = "avif")]
            include_str!("avif/decode.rs"),
            include_str!("main.rs"),
        ];
        for src in sources {
            for m in src.match_indices("\"OXIMG_") {
                let rest = &src[m.0 + 1..];
                // Only bare OXIMG_XXX string literals count — prose
                // that merely mentions a knob (error messages) is not
                // an env read.
                let end = rest
                    .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                    .unwrap_or(rest.len());
                if !rest[end..].starts_with('"') || end <= "OXIMG_".len() {
                    continue;
                }
                let name = &rest[..end];
                // main.rs startup settings are documented separately.
                let startup = [
                    "OXIMG_LOG",
                    "OXIMG_KEY",
                    "OXIMG_SALT",
                    "OXIMG_SOURCE_BASE_URL",
                    "OXIMG_AUTO_FORMAT",
                    "OXIMG_PAR",
                ];
                assert!(
                    KNOBS.contains(&name) || startup.contains(&name),
                    "{name} is read but missing from the config inventory"
                );
            }
        }
    }
}
