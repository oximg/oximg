//! The Cloudflare Images option-list grammar (issue #9):
//! `width=750,quality=80` as one path segment, so URLs built for
//! Cloudflare Images (or for a CDN speaking the same shape) survive a
//! migration without a rewrite layer. Only the grammar is adopted —
//! the option set is the minimal one that covers real migrations
//! (width/height/quality/format), and everything unknown is a named
//! 400 rather than Cloudflare's silent ignore: a dropped `fit=cover`
//! changes the output, and fail-closed is this codebase's rule.

use oximg::pipeline::ImageFormat;

/// One parsed option list. Dimensions use the zero-axis convention
/// (0 = unconstrained), which `width=N` alone maps onto exactly.
#[derive(Debug)]
pub struct ResizeOptions {
    pub width: u32,
    pub height: u32,
    pub quality: Option<u8>,
    /// `Some` only for an explicit concrete `format=`; `format=auto`
    /// and an absent `format` both mean "negotiate, else source" —
    /// exactly what a bare positional URL does.
    pub format: Option<ImageFormat>,
}

/// Parse `key=value,key=value`. Errors are client-facing 400 bodies:
/// they name the offending key so a migration can find the call site.
pub fn parse(options: &str) -> Result<ResizeOptions, String> {
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut quality: Option<u8> = None;
    let mut format: Option<Option<ImageFormat>> = None;

    for part in options.split(',') {
        let Some((key, value)) = part.split_once('=') else {
            return Err(format!("malformed option {part:?} (expected key=value)"));
        };
        fn set<T>(slot: &mut Option<T>, key: &str, value: T) -> Result<(), String> {
            if slot.replace(value).is_some() {
                return Err(format!("duplicate option {key:?}"));
            }
            Ok(())
        }
        match key {
            "width" | "height" => {
                let dim: u32 = value
                    .parse()
                    .ok()
                    .filter(|d| (1..=8192).contains(d))
                    .ok_or_else(|| format!("invalid {key} {value:?} (1-8192)"))?;
                set(
                    if key == "width" {
                        &mut width
                    } else {
                        &mut height
                    },
                    key,
                    dim,
                )?;
            }
            "quality" => {
                let q: u8 = value
                    .parse()
                    .ok()
                    .filter(|q| (1..=100).contains(q))
                    .ok_or_else(|| format!("invalid quality {value:?} (1-100)"))?;
                set(&mut quality, key, q)?;
            }
            "format" => {
                let f = match value {
                    "auto" => None,
                    _ => match ImageFormat::from_token(value) {
                        Some(ImageFormat::Avif) if cfg!(not(feature = "avif")) => {
                            return Err("avif output is not enabled in this build".into());
                        }
                        Some(f) => Some(f),
                        None => {
                            return Err(format!(
                                "invalid format {value:?} (jpeg|png|webp|avif|auto)"
                            ));
                        }
                    },
                };
                set(&mut format, key, f)?;
            }
            _ => return Err(format!("unknown option {key:?}")),
        }
    }
    if width.is_none() && height.is_none() {
        return Err("at least one of width/height is required".into());
    }
    Ok(ResizeOptions {
        width: width.unwrap_or(0),
        height: height.unwrap_or(0),
        quality,
        format: format.flatten(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> ResizeOptions {
        parse(s).expect(s)
    }

    #[test]
    fn accepts_the_migration_grammar() {
        // The exact shape the motivating frontend emits.
        let o = ok("width=750,quality=80");
        assert_eq!((o.width, o.height), (750, 0), "height unconstrained");
        assert_eq!(o.quality, Some(80));
        assert_eq!(o.format, None);

        let o = ok("height=1024");
        assert_eq!((o.width, o.height), (0, 1024));

        let o = ok("width=100,height=200,quality=1,format=webp");
        assert_eq!((o.width, o.height), (100, 200));
        assert_eq!(o.quality, Some(1));
        assert_eq!(o.format, Some(ImageFormat::Webp));

        // Order does not matter to the parse.
        let a = ok("width=750,quality=80");
        let b = ok("quality=80,width=750");
        assert_eq!((a.width, a.quality), (b.width, b.quality));

        // format=auto and absent format both mean "negotiate".
        assert_eq!(ok("width=1,format=auto").format, None);
        #[cfg(feature = "avif")]
        assert_eq!(ok("width=1,format=avif").format, Some(ImageFormat::Avif));
    }

    /// Every rejection names its cause: unknown keys are 400 (a
    /// silently ignored fit=cover would change the output — the
    /// documented divergence from Cloudflare), duplicates and
    /// malformed pairs are 400, ranges are enforced, and an option
    /// list without any dimension is not a resize.
    #[test]
    fn rejects_with_the_offending_key_named() {
        for (input, needle) in [
            ("width=750,fit=cover", "fit"),
            ("width=750,width=100", "duplicate"),
            ("width=750,quality", "malformed"),
            ("width=750,,quality=80", "malformed"),
            ("width=0", "width"),
            ("width=9000", "width"),
            ("height=0", "height"),
            ("width=abc", "width"),
            ("width=1,quality=0", "quality"),
            ("width=1,quality=101", "quality"),
            ("width=1,format=gif", "format"),
            ("width=1,format=jxl", "format"),
            ("quality=80", "width/height"),
            ("", "malformed"),
        ] {
            let err = parse(input).expect_err(input);
            assert!(err.contains(needle), "{input:?}: {err:?} lacks {needle:?}");
        }
        #[cfg(not(feature = "avif"))]
        assert!(parse("width=1,format=avif").unwrap_err().contains("avif"));
    }
}
