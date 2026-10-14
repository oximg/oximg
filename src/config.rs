//! Every runtime knob the pipeline reads, resolved once at first use
//! and cached for the process lifetime. One inventory, one caching
//! rule ("set at startup"), typed fields instead of scattered string
//! compares — and a test pinning every knob to its README entry.
//!
//! Knobs with a per-call `Params` override are merged (override >
//! env > default) in `pipeline/resolved.rs`: when adding one, give it
//! a `Resolved` field there — do not write a new resolver fn.
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
    /// OXIMG_DCT_MARGIN: decode-size headroom over the target, and the
    /// switch that turns shrink-on-load on at all. `None` (the default)
    /// decodes at full size and hands the whole reduction to the
    /// resampler.
    ///
    /// Shrink-on-load is a **speed** knob, not a quality one: it was
    /// the default at 1.7 until measurement showed it only ever costs
    /// quality against the linear-light reference the resize is built
    /// around (an sRGB reference favors 1/4 and 1/2; see issue #60).
    /// Against a lossless ground truth and that reference, full decode
    /// is the best cell at every ratio from 2x to 14x; 3/8, which 1.7 picks on
    /// a 5.3x downscale, costs 6.4 SSIMULACRA2 points on average for
    /// the same output size and the same bytes (bench/quality/
    /// dct_sweep.py, 100 DIV2K photographs). Set it to buy throughput
    /// back on large sources, knowing what it spends.
    pub dct_margin: Option<f64>,
    /// OXIMG_LINEAR_SHRINK ("0" disables): with OXIMG_DCT_MARGIN unset,
    /// a JPEG reduced 4x or more decodes luma at 1/2 scale through a
    /// linear-light 2x2 average of the full IDCT (issue #60), leaving
    /// at least 2x for the resampler.
    pub linear_shrink: bool,
    /// OXIMG_JPEG_PROGRESSIVE ("0" selects sequential jpegli, SOF1).
    pub jpegli_progressive: bool,
    /// OXIMG_FLATTEN_BG: alpha→JPEG flatten background, RRGGBB hex.
    pub flatten_bg: [u8; 3],
    /// OXIMG_PNG_EFFORT: fastest / fast / balanced / high. `None` =
    /// unset, so the effective default can depend on the path: `fast`
    /// for lossless output, `balanced` when quantization is active
    /// (see `Resolved::png_compression`).
    pub png_compression: Option<png::Compression>,
    /// OXIMG_PNG_QUANTIZE ("1" enables palette quantization for opaque
    /// PNG output; off by default — silent quality loss on a lossless
    /// format must be a deliberate operator choice).
    pub png_quantize: bool,
    /// OXIMG_PNG_QUANTIZE_COLORS: palette size, 2-256.
    pub png_quantize_colors: u16,
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
    /// OXIMG_UPSTREAM_CONNECT_TIMEOUT: seconds to establish the origin
    /// connection (remote-source path).
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub upstream_connect_timeout: u64,
    /// OXIMG_UPSTREAM_TIMEOUT: seconds for the whole origin fetch —
    /// the bound on how long a stalled upstream can hold a CPU permit.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub upstream_timeout: u64,
    /// OXIMG_MAX_DECODED_BYTES: cap on what a single decode is
    /// estimated to allocate. `None` = unset (off): the estimate is
    /// still computed and exposed, so a cap can be derived from a real
    /// corpus before being enforced.
    pub max_decoded_bytes: Option<u64>,
    /// OXIMG_LOG_DECODED_BYTES_ABOVE: report (and still serve) any
    /// decode whose estimate exceeds this. Orthogonal to the cap: the
    /// cap refuses and names what it refused, this names without
    /// refusing — the only way to learn which sources are expensive
    /// before choosing a limit (issue #19).
    pub log_decoded_bytes_above: Option<u64>,
    /// OXIMG_MAX_SRC_PIXELS: decoded-size cap (w*h), enforced after
    /// each format's header parse and before any pixel-sized
    /// allocation — compressed-size caps do not bound decoded size.
    pub max_src_pixels: u64,
    /// OXIMG_GIF_ANIMATION ("0" renders every animated GIF as its still
    /// first frame instead of animated WebP).
    pub gif_animation: bool,
    /// OXIMG_MAX_ANIM_FRAMES: source frames an animation may carry
    /// before it degrades to a still. Bounds the decode+composite half
    /// of the work, which no output size reduces.
    pub max_anim_frames: usize,
    /// OXIMG_MAX_ANIM_WORK: encoded frames x post-resize frame area, in
    /// pixels — the product that predicts encode time, which is what
    /// dominates an animation (docs/gif-evaluation.md §5). Over it, the
    /// request degrades to a still rather than failing.
    pub max_anim_work: u64,
    /// OXIMG_ANIM_FRAME_STEP: encode every Nth frame (1 = all of them).
    /// Total duration is preserved, so decimation costs smoothness, not
    /// fidelity — off by default for that reason.
    pub anim_frame_step: usize,
}

/// Pipeline knob inventory, pinned to the README and
/// `docs/features/knobs.md` by `knobs_are_documented`.
#[cfg(test)]
const KNOBS: &[&str] = &[
    "OXIMG_TIMING",
    "OXIMG_RESIZE",
    "OXIMG_RESIZE_BACKEND",
    "OXIMG_AUTO_ROTATE",
    "OXIMG_ICC",
    "OXIMG_DCT_MARGIN",
    "OXIMG_LINEAR_SHRINK",
    "OXIMG_JPEG_PROGRESSIVE",
    "OXIMG_FLATTEN_BG",
    "OXIMG_PNG_EFFORT",
    "OXIMG_PNG_QUANTIZE",
    "OXIMG_PNG_QUANTIZE_COLORS",
    "OXIMG_WEBP_QUALITY",
    "OXIMG_WEBP_EFFORT",
    "OXIMG_WEBP_DECODE_THREADS",
    "OXIMG_AVIF_QUALITY",
    "OXIMG_AVIF_ALPHA_QUALITY",
    "OXIMG_AVIF_SPEED",
    "OXIMG_AVIF_DECODE_THREADS",
    "OXIMG_MAX_SOURCE_BYTES",
    "OXIMG_MAX_SRC_PIXELS",
    "OXIMG_GIF_ANIMATION",
    "OXIMG_MAX_ANIM_FRAMES",
    "OXIMG_MAX_ANIM_WORK",
    "OXIMG_ANIM_FRAME_STEP",
    "OXIMG_MAX_DECODED_BYTES",
    "OXIMG_LOG_DECODED_BYTES_ABOVE",
    "OXIMG_UPSTREAM_CONNECT_TIMEOUT",
    "OXIMG_UPSTREAM_TIMEOUT",
    "OXIMG_GCS_ENDPOINT",
    "OXIMG_S3_ENDPOINT",
    "OXIMG_S3_PATH_STYLE",
    "OXIMG_OVERLAP",
];

/// Server-startup env (live in `main.rs`), documented in the README
/// separately and in `docs/features/knobs.md`.
#[cfg(test)]
const STARTUP: &[&str] = &[
    "OXIMG_LOG",
    "OXIMG_KEY",
    "OXIMG_SALT",
    "OXIMG_SOURCE_BASE_URL",
    "OXIMG_AUTO_FORMAT",
    "OXIMG_PAR",
    "OXIMG_METRICS",
    "OXIMG_OPTIONS_PREFIX",
    "OXIMG_WORKERS",
    "OXIMG_FETCH_CONCURRENCY",
    "OXIMG_BIND",
];

/// Process env without the `OXIMG_` prefix; still in the feature map.
#[cfg(test)]
const PROCESS: &[&str] = &[
    "PORT",
    "IMAGES_DIR",
    "QUALITY",
    "PRESET",
    "GCE_METADATA_HOST",
    "AWS_REGION",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GLIBC_TUNABLES",
];

/// A knob as `validate` sees it: trimmed, and blank reads as unset.
/// Every reader goes through this, so a value that passed validation
/// cannot then be ignored over surrounding whitespace — `" 90"` was
/// accepted as a quality and then silently served at the default.
pub(crate) fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parsed<T: std::str::FromStr>(name: &str) -> Option<T> {
    var(name).and_then(|v| v.parse().ok())
}

/// OXIMG_PNG_EFFORT: a level name, or a zlib-style 0-9 — the numeric
/// scale zlib, pngcrush and ImageMagick use, and what a reader from
/// that ecosystem types first (issue #8). The numbers follow what the
/// levels are underneath: `balanced` is zlib's default 6 and `high`
/// its best 9, while `fast`/`fastest` are fdeflate modes quicker than
/// any zlib level, so they take the low end. There is no stored-only
/// level, so 0 is the fastest one there is.
fn png_effort(v: &str) -> Option<png::Compression> {
    Some(match v {
        "fastest" | "0" | "1" => png::Compression::Fastest,
        "fast" | "2" | "3" | "4" | "5" => png::Compression::Fast,
        // Balanced spends ~15ms/request more than Fast to shave ~14%
        // of the file; Fast still undercuts libvips' default output
        // size.
        "balanced" | "6" | "7" | "8" => png::Compression::Balanced,
        "high" | "9" => png::Compression::High,
        _ => return None,
    })
}

/// Strict startup validation for the server binary: every knob that
/// is *set* must parse and sit in range — a typo in a limit must not
/// silently fail open to a default (the fail-closed precedent set by
/// the signing config). The library-facing `config()` stays lenient
/// so embedding never aborts a host process over env noise.
///
/// One knob here is lenient by design: `OXIMG_PNG_EFFORT` warns and
/// falls back rather than failing (see its arm below). Knobs that
/// warn instead of failing are listed in docs/features/knobs.md.
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
        "OXIMG_LINEAR_SHRINK",
        "OXIMG_WEBP_DECODE_THREADS",
        "OXIMG_PNG_QUANTIZE",
        "OXIMG_GIF_ANIMATION",
        "OXIMG_S3_PATH_STYLE",
    ] {
        one_of(b, &["0", "1"])?;
    }
    num("OXIMG_PNG_QUANTIZE_COLORS", 2i64, 256)?;
    one_of("OXIMG_OVERLAP", &["0", "1", "auto"])?;
    one_of("OXIMG_RESIZE", &["srgb", "linear"])?;
    one_of("OXIMG_RESIZE_BACKEND", &["fir", "kernel"])?;
    // Lenient, like OXIMG_LOG (issue #46): effort only trades encode
    // time against file size, never what is produced, so an unknown
    // value — `10`, rounding zlib's best up, is the likely one — warns
    // and falls back to the unset default (`config()` already reads it
    // as `None`) instead of crash-looping a rollout over a typo.
    if let Some(v) = set("OXIMG_PNG_EFFORT")
        && png_effort(v.trim()).is_none()
    {
        eprintln!(
            "oximg: warning: OXIMG_PNG_EFFORT={v:?} is not one of fastest, fast, balanced, \
             high, or a zlib-style level 0-9; using the default, as if unset"
        );
    }
    one_of("OXIMG_METRICS", &["0", "1"])?;
    num("OXIMG_DCT_MARGIN", 1.0f64, 8.0)?;
    num("OXIMG_WEBP_QUALITY", 0.0f32, 100.0)?;
    num("OXIMG_WEBP_EFFORT", 0i64, 6)?;
    num("OXIMG_AVIF_QUALITY", 0i64, 100)?;
    num("OXIMG_AVIF_ALPHA_QUALITY", 0i64, 100)?;
    num("OXIMG_AVIF_SPEED", 0i64, 13)?;
    num("OXIMG_AVIF_DECODE_THREADS", 1i64, 64)?;
    num("OXIMG_MAX_SOURCE_BYTES", 1u64, u64::MAX)?;
    num("OXIMG_MAX_SRC_PIXELS", 1u64, u64::MAX)?;
    num("OXIMG_MAX_ANIM_FRAMES", 1u64, u64::MAX)?;
    num("OXIMG_MAX_ANIM_WORK", 1u64, u64::MAX)?;
    // A step above the frame cap could only ever emit one frame, which
    // is a still with extra steps; 64 is far past any useful decimation.
    num("OXIMG_ANIM_FRAME_STEP", 1u64, 64)?;
    // A cap under a mebibyte cannot admit any real image; treating it
    // as a typo is friendlier than 413ing every request.
    num("OXIMG_MAX_DECODED_BYTES", 1u64 << 20, u64::MAX)?;
    // No mebibyte floor here: unlike the cap, a small threshold is a
    // legitimate "log everything" debug mode rather than a footgun.
    num("OXIMG_LOG_DECODED_BYTES_ABOVE", 1u64, u64::MAX)?;
    num("OXIMG_UPSTREAM_CONNECT_TIMEOUT", 1u64, 3600)?;
    num("OXIMG_UPSTREAM_TIMEOUT", 1u64, 3600)?;
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
        linear_light: var("OXIMG_RESIZE").as_deref() != Some("srgb"),
        fir_backend: var("OXIMG_RESIZE_BACKEND").as_deref() == Some("fir"),
        auto_rotate: var("OXIMG_AUTO_ROTATE").as_deref() != Some("0"),
        icc_passthrough: var("OXIMG_ICC").as_deref() != Some("0"),
        dct_margin: parsed("OXIMG_DCT_MARGIN"),
        linear_shrink: var("OXIMG_LINEAR_SHRINK").as_deref() != Some("0"),
        jpegli_progressive: var("OXIMG_JPEG_PROGRESSIVE").as_deref() != Some("0"),
        flatten_bg: var("OXIMG_FLATTEN_BG")
            .and_then(|v| {
                let v = v.trim_start_matches('#');
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
        png_compression: var("OXIMG_PNG_EFFORT").and_then(|v| png_effort(&v)),
        png_quantize: var("OXIMG_PNG_QUANTIZE").as_deref() == Some("1"),
        png_quantize_colors: parsed::<u16>("OXIMG_PNG_QUANTIZE_COLORS")
            .filter(|c| (2..=256).contains(c))
            .unwrap_or(256),
        webp_quality: parsed("OXIMG_WEBP_QUALITY").unwrap_or(75.0),
        webp_effort: parsed("OXIMG_WEBP_EFFORT").unwrap_or(2),
        webp_decode_threads: var("OXIMG_WEBP_DECODE_THREADS").as_deref() != Some("0"),
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
        gif_animation: var("OXIMG_GIF_ANIMATION").as_deref() != Some("0"),
        max_anim_frames: parsed("OXIMG_MAX_ANIM_FRAMES").unwrap_or(200),
        // 8 Mpx of post-resize frame area: the corpus in
        // docs/gif-evaluation.md §5 puts its worst in-budget file
        // (hd_1280x720_mars, 26 frames into a 512 box, ~6.8 Mpx) at
        // 517 ms, and its worst file overall (web_480x270_docu, ~34 Mpx)
        // at 3.2 s — which this refuses, serving a still instead.
        max_anim_work: parsed("OXIMG_MAX_ANIM_WORK").unwrap_or(8_000_000),
        anim_frame_step: parsed::<usize>("OXIMG_ANIM_FRAME_STEP")
            .filter(|s| *s >= 1)
            .unwrap_or(1),
        max_decoded_bytes: parsed("OXIMG_MAX_DECODED_BYTES").filter(|b| *b >= (1 << 20)),
        log_decoded_bytes_above: parsed("OXIMG_LOG_DECODED_BYTES_ABOVE").filter(|b| *b >= 1),
        upstream_connect_timeout: parsed("OXIMG_UPSTREAM_CONNECT_TIMEOUT").unwrap_or(5),
        upstream_timeout: parsed("OXIMG_UPSTREAM_TIMEOUT").unwrap_or(30),
    })
}

#[cfg(test)]
mod tests {
    use super::{KNOBS, PROCESS, STARTUP, png_effort};
    use crate::pipeline::ImageFormat;
    use std::collections::{HashMap, HashSet};

    /// Every name in KNOBS+STARTUP+PROCESS must appear in the README
    /// and in knobs.md. Every OXIMG_* the crate reads must be in
    /// KNOBS or STARTUP. Drift here is how #36's review rounds started.
    #[test]
    fn knobs_are_documented() {
        let readme = include_str!("../README.md");
        let map = include_str!("../docs/features/knobs.md");
        let inventory: HashSet<&str> = KNOBS
            .iter()
            .chain(STARTUP)
            .chain(PROCESS)
            .copied()
            .collect();
        for k in &inventory {
            assert!(readme.contains(k), "{k} is not documented in README.md");
        }
        let documented = knob_table_names(map);
        assert_eq!(
            documented,
            inventory,
            "docs/features/knobs.md table cells != KNOBS+STARTUP+PROCESS\nextra in map: {:?}\nmissing from map: {:?}",
            documented.difference(&inventory).collect::<Vec<_>>(),
            inventory.difference(&documented).collect::<Vec<_>>(),
        );
        // Inventory completeness: scan our own sources for env reads.
        let sources = [
            include_str!("config.rs"),
            include_str!("pipeline/mod.rs"),
            include_str!("pipeline/jpeg.rs"),
            include_str!("pipeline/fuse.rs"),
            include_str!("pipeline/formats.rs"),
            include_str!("pipeline/gif.rs"),
            include_str!("pipeline/resolved.rs"),
            #[cfg(feature = "server")]
            include_str!("pipeline/gcs.rs"),
            #[cfg(feature = "server")]
            include_str!("pipeline/s3.rs"),
            include_str!("pipeline/encode.rs"),
            #[cfg(feature = "avif")]
            include_str!("avif/encode.rs"),
            #[cfg(feature = "avif")]
            include_str!("avif/decode.rs"),
            include_str!("main.rs"),
            include_str!("cli.rs"),
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
                assert!(
                    KNOBS.contains(&name) || STARTUP.contains(&name),
                    "{name} is read but missing from the config inventory"
                );
            }
        }
        // Do not scan config.rs: PROCESS names appear in the inventory
        // itself. Always include gcs.rs and s3.rs so
        // --no-default-features still sees GCE_METADATA_HOST and the
        // AWS_* names.
        let process_sources = [
            include_str!("main.rs"),
            include_str!("cli.rs"),
            include_str!("pipeline/gcs.rs"),
            include_str!("pipeline/s3.rs"),
        ];
        let mut reads: HashSet<&str> = HashSet::new();
        for src in process_sources {
            for name in process_env_reads(src) {
                reads.insert(name);
            }
        }
        let process: HashSet<&str> = PROCESS.iter().copied().collect();
        assert_eq!(
            reads,
            process,
            "non-OXIMG_ env reads != PROCESS\nextra in code: {:?}\nmissing from PROCESS: {:?}",
            reads.difference(&process).collect::<Vec<_>>(),
            process.difference(&reads).collect::<Vec<_>>(),
        );
    }

    /// HTTP statuses and ErrorKind names in docs/features/errors.md
    /// must match the server/library contract. A missing row is how
    /// `@avif` without the feature drifted to "422" in the map.
    #[test]
    fn feature_map_errors() {
        let map = include_str!("../docs/features/errors.md");
        let from_code: HashSet<&str> = error_kind_variants(include_str!("pipeline/error.rs"))
            .into_iter()
            .collect();
        let kind_http_doc = kind_http_pairs(map);
        let from_map: HashSet<&str> = kind_http_doc.keys().copied().collect();
        assert_eq!(
            from_map,
            from_code,
            "docs/features/errors.md Kind column != ErrorKind\nextra in map: {:?}\nmissing from map: {:?}",
            from_map.difference(&from_code).collect::<Vec<_>>(),
            from_code.difference(&from_map).collect::<Vec<_>>(),
        );
        let main = include_str!("main.rs");
        let kind_http_code = error_kind_http_from_main(main);
        for (kind, status) in &kind_http_doc {
            assert_eq!(
                kind_http_code.get(kind),
                Some(status),
                "{kind} documented as {status}, error_response maps {:?}",
                kind_http_code.get(kind)
            );
        }
        assert_eq!(
            unknown_http_row(map),
            Some(500),
            "unknown (non_exhaustive) Kind row must be HTTP 500"
        );
        let http = map
            .split("## Library")
            .next()
            .expect("## Library heading in docs/features/errors.md");
        let mut from_code_status: HashSet<u16> = HashSet::new();
        for (i, _) in main.match_indices("StatusCode::") {
            let rest = &main[i + "StatusCode::".len()..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphabetic() || c == '_'))
                .unwrap_or(rest.len());
            let ident = &rest[..end];
            if ident.is_empty() {
                continue;
            }
            from_code_status.insert(status_from_ident(ident));
        }
        // 200 (success) and 405 (axum method-router) are not spelled
        // StatusCode::OK / METHOD_NOT_ALLOWED in main.rs.
        from_code_status.insert(200);
        from_code_status.insert(405);
        let from_http = http_status_column(http);
        assert_eq!(
            from_http,
            from_code_status,
            "HTTP section statuses != StatusCode uses in main.rs\nextra in map: {:?}\nmissing from map: {:?}",
            from_http.difference(&from_code_status).collect::<Vec<_>>(),
            from_code_status.difference(&from_http).collect::<Vec<_>>(),
        );
    }

    // Variant identifiers of `pub enum ErrorKind` (docs/attributes skipped).
    fn error_kind_variants(src: &str) -> Vec<&str> {
        let start = src.find("pub enum ErrorKind").expect("pub enum ErrorKind");
        let body = src[start..]
            .find('{')
            .map(|i| &src[start + i + 1..])
            .expect("ErrorKind body");
        let mut kinds = Vec::new();
        let mut depth = 1i32;
        for line in body.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with("//") || t.starts_with("#[") {
                continue;
            }
            depth += t.bytes().filter(|&c| c == b'{').count() as i32;
            depth -= t.bytes().filter(|&c| c == b'}').count() as i32;
            if depth <= 0 {
                break;
            }
            let end = t
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(t.len());
            let name = &t[..end];
            if !name.is_empty() && name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                kinds.push(name);
            }
        }
        assert!(
            !kinds.is_empty(),
            "parsed no ErrorKind variants from pipeline/error.rs"
        );
        kinds
    }

    fn status_from_ident(ident: &str) -> u16 {
        match ident {
            "NO_CONTENT" => 204,
            "BAD_REQUEST" => 400,
            "FORBIDDEN" => 403,
            "NOT_FOUND" => 404,
            "PAYLOAD_TOO_LARGE" => 413,
            "UNPROCESSABLE_ENTITY" => 422,
            "INTERNAL_SERVER_ERROR" => 500,
            "BAD_GATEWAY" => 502,
            "SERVICE_UNAVAILABLE" => 503,
            "GATEWAY_TIMEOUT" => 504,
            other => panic!(
                "StatusCode::{other} has no mapping in feature_map_errors; add the mapping and a docs/features/errors.md row"
            ),
        }
    }

    /// `from_token`'s accepted and refused tables must appear in the
    /// formats map, and every non-Gif ImageFormat must have a token.
    #[test]
    fn feature_map_format_tokens() {
        let map = include_str!("../docs/features/formats.md");
        let accepted_line = map
            .lines()
            .find(|l| l.starts_with("Accepted `@{fmt}` tokens:"))
            .expect("Accepted @{fmt} tokens line in docs/features/formats.md");
        let accepted_doc: HashSet<&str> =
            backtick_token_idents(accepted_line).into_iter().collect();
        let accepted_code: HashSet<&str> = ImageFormat::OUTPUT_TOKENS
            .iter()
            .map(|&(tok, _)| tok)
            .collect();
        assert_eq!(
            accepted_doc,
            accepted_code,
            "Accepted @{{fmt}} list != OUTPUT_TOKENS\nextra in map: {:?}\nmissing from map: {:?}",
            accepted_doc.difference(&accepted_code).collect::<Vec<_>>(),
            accepted_code.difference(&accepted_doc).collect::<Vec<_>>(),
        );
        let hint = ImageFormat::output_token_hint();
        for &(tok, fmt) in ImageFormat::OUTPUT_TOKENS {
            assert_eq!(ImageFormat::from_token(tok), Some(fmt), "{tok}");
            assert!(
                hint.split('|').any(|t| t == tok),
                "output_token_hint {hint:?} missing {tok}"
            );
        }
        let refused_line = map
            .lines()
            .find(|l| l.contains("are refused"))
            .expect("refused-token sentence in docs/features/formats.md");
        let refused_doc: HashSet<&str> = at_tokens(refused_line).into_iter().collect();
        let refused_code: HashSet<&str> =
            ImageFormat::REFUSED_OUTPUT_TOKENS.iter().copied().collect();
        assert_eq!(
            refused_doc,
            refused_code,
            "refused `@{{tok}}` set != REFUSED_OUTPUT_TOKENS\nextra in map: {:?}\nmissing from map: {:?}",
            refused_doc.difference(&refused_code).collect::<Vec<_>>(),
            refused_code.difference(&refused_doc).collect::<Vec<_>>(),
        );
        assert!(
            accepted_code.is_disjoint(&refused_code),
            "OUTPUT_TOKENS and REFUSED_OUTPUT_TOKENS overlap"
        );
        for tok in ImageFormat::REFUSED_OUTPUT_TOKENS {
            assert_eq!(ImageFormat::from_token(tok), None, "{tok}");
        }
        // Exhaustive: a new ImageFormat variant fails to compile here.
        let mut jpeg = false;
        let mut png = false;
        let mut webp = false;
        let mut avif = false;
        for &(_, fmt) in ImageFormat::OUTPUT_TOKENS {
            match fmt {
                ImageFormat::Jpeg => jpeg = true,
                ImageFormat::Png => png = true,
                ImageFormat::Webp => webp = true,
                ImageFormat::Avif => avif = true,
                ImageFormat::Gif => panic!("Gif must not appear in OUTPUT_TOKENS"),
            }
        }
        assert!(
            jpeg && png && webp && avif,
            "OUTPUT_TOKENS is missing an encodable ImageFormat"
        );
    }

    fn backtick_inners(s: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(i) = rest.find('`') {
            rest = &rest[i + 1..];
            let Some(j) = rest.find('`') else { break };
            out.push(&rest[..j]);
            rest = &rest[j + 1..];
        }
        out
    }

    fn is_env_ident(s: &str) -> bool {
        let mut chars = s.chars();
        matches!(chars.next(), Some('A'..='Z'))
            && s.len() > 1
            && s.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    }

    fn knob_table_names(map: &str) -> HashSet<&str> {
        map.lines()
            .filter(|l| l.starts_with("| `"))
            .flat_map(|l| backtick_inners(l.split('|').nth(1).unwrap_or("")))
            .filter(|s| is_env_ident(s))
            .collect()
    }

    fn kind_http_pairs(map: &str) -> HashMap<&str, u16> {
        let lib = map
            .split("## Library")
            .nth(1)
            .expect("## Library heading in docs/features/errors.md");
        let mut pairs = HashMap::new();
        for line in lib.lines() {
            let Some(rest) = line.trim().strip_prefix("| `") else {
                continue;
            };
            let Some(end) = rest.find('`') else { continue };
            let name = &rest[..end];
            if !(name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && name.chars().all(|c| c.is_ascii_alphanumeric())
                && name.chars().any(|c| c.is_ascii_lowercase()))
            {
                continue;
            }
            let after = rest[end + 1..].trim_start_matches([' ', '|']);
            let status_cell = after.split('|').next().unwrap_or("").trim();
            let status: u16 = status_cell.parse().unwrap_or_else(|_| {
                panic!("{name} Kind row has no HTTP status, got {status_cell:?}")
            });
            pairs.insert(name, status);
        }
        pairs
    }

    fn unknown_http_row(map: &str) -> Option<u16> {
        map.lines().find_map(|line| {
            let t = line.trim();
            if !t.contains("non_exhaustive") {
                return None;
            }
            t.split('|').nth(2).and_then(|c| c.trim().parse().ok())
        })
    }

    fn http_status_column(http: &str) -> HashSet<u16> {
        http.lines()
            .filter_map(|line| {
                let t = line.trim();
                let rest = t.strip_prefix('|')?;
                let cell = rest.split('|').next()?.trim();
                cell.parse().ok()
            })
            .collect()
    }

    fn error_kind_http_from_main(src: &str) -> HashMap<&str, u16> {
        let start = src
            .find("fn error_response")
            .expect("fn error_response in main.rs");
        let slice = &src[start..];
        let end = slice[1..]
            .find("\nfn ")
            .map(|i| i + 1)
            .unwrap_or(slice.len());
        let body = &slice[..end];
        let mut map = HashMap::new();
        let mut i = 0;
        while let Some(p) = body[i..].find("ErrorKind::") {
            let rest = &body[i + p + "ErrorKind::".len()..];
            let name_end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let name = &rest[..name_end];
            if let Some(s) = rest.find("StatusCode::") {
                let ident_rest = &rest[s + "StatusCode::".len()..];
                let ident_end = ident_rest
                    .find(|c: char| !(c.is_ascii_alphabetic() || c == '_'))
                    .unwrap_or(ident_rest.len());
                let ident = &ident_rest[..ident_end];
                if !name.is_empty() && !ident.is_empty() {
                    map.insert(name, status_from_ident(ident));
                }
            }
            i += p + "ErrorKind::".len();
        }
        map
    }

    fn backtick_token_idents(s: &str) -> Vec<&str> {
        backtick_inners(s)
            .into_iter()
            .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_lowercase()))
            .collect()
    }

    /// `env_or("PORT"` / `std::env::var("IMAGES_DIR"` — ALL_CAPS names
    /// that are not `OXIMG_*`.
    fn process_env_reads(src: &str) -> Vec<&str> {
        let mut names = Vec::new();
        for needle in ["env_or(\"", "env::var(\""] {
            let mut i = 0;
            while let Some(p) = src[i..].find(needle) {
                let rest = &src[i + p + needle.len()..];
                let end = rest
                    .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                    .unwrap_or(rest.len());
                if rest[end..].starts_with('"') && end > 0 {
                    let name = &rest[..end];
                    if is_env_ident(name) && !name.starts_with("OXIMG_") {
                        names.push(name);
                    }
                }
                i += p + needle.len();
            }
        }
        names
    }

    fn at_tokens(map: &str) -> Vec<&str> {
        backtick_inners(map)
            .into_iter()
            .filter_map(|inner| {
                inner.strip_prefix('@').and_then(|tok| {
                    (!tok.is_empty() && tok.chars().all(|c| c.is_ascii_lowercase())).then_some(tok)
                })
            })
            .collect()
    }

    /// Issue #8: every zlib-style level lands on the named level with
    /// the same deflate underneath (6 = zlib default = `balanced`,
    /// 9 = zlib best = `high`); anything else is `None`, which
    /// `validate` turns into a warning and the unset default.
    #[test]
    fn png_effort_accepts_names_and_zlib_levels() {
        use png::Compression as C;
        let level = |v: &str| match png_effort(v) {
            Some(C::Fastest) => "fastest",
            Some(C::Fast) => "fast",
            Some(C::Balanced) => "balanced",
            Some(C::High) => "high",
            Some(other) => panic!("{v:?} mapped to unexpected {other:?}"),
            None => "unknown",
        };
        for name in ["fastest", "fast", "balanced", "high"] {
            assert_eq!(level(name), name);
        }
        let by_number: Vec<&str> = (0..=9).map(|n| level(&n.to_string())).collect();
        assert_eq!(
            by_number,
            [
                "fastest", "fastest", "fast", "fast", "fast", "fast", "balanced", "balanced",
                "balanced", "high"
            ]
        );
        for bad in ["10", "-1", "09", "1.5", "max", "High", ""] {
            assert_eq!(level(bad), "unknown", "{bad:?}");
        }
    }
}
