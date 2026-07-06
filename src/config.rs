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
    /// OXIMG_MAX_SOURCE_BYTES: remote-source download cap.
    pub max_source_bytes: u64,
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
    "OXIMG_OVERLAP",
];

fn parsed<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
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
